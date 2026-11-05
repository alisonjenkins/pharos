# ADR-0058: Native clients fetch media with a Media-scoped token

- **Status:** Accepted
- **Date:** 2026-11-04T23:30:00Z
- **Deciders:** Alison Jenkins

## Context

A native client (the Android TV app, spec 025) loads posters and plays media
through a WebView. That origin has no `pharos_media` cookie (ADR-0034), so the
spike appended `?api_key=<device token>` to every image and stream URL. The
device token is Full scope: in a URL it lands in browser history, proxy logs
and HLS playlists, and whoever reads it can call the whole API as the user.
ADR-0042 already has restricted token scopes (Playback, for cast receivers)
enforced through one allowlist.

## Decision

We add `TokenScope::Media`, stored as `media`. A Media token may call only GET
and HEAD on `/items/{id}/images/` (prefix), `/videos/{id}/` (prefix) and
`/audio/{id}/` (prefix), listed in `MEDIA_ALLOWLIST` beside
`PLAYBACK_ALLOWLIST` and enforced by `scope_permits` on every path that honours
a token. `user_for_token` still refuses every non-Full scope, so a Media token
cannot resume a native session or call the native API.

Schema version 15 adds `Session.mediaToken @14`, answering
`MediaToken { token, expiresAtMs }`. It issues the token with
`issue_with_ttl(user, "pharos-native:media", media_token_ttl_secs, Media)`;
`[server.native_api].media_token_ttl_secs` is a NonZeroU32 defaulting to 21600
(6 h), at most 86400 (a day; a larger value fails config load). A revoked or ended session answers `Error.kind.revoked`. The client asks
again before `expiresAtMs`. The counter `pharos_media_token_issued_total{outcome}`
(`ok`, `store_unavailable`, `rejected`) and an info log with the user and the
token id prefix make issuing visible; the secret is never logged. Refusals
count in `pharos_token_scope_refused_total{scope="media"}`.

## Consequences

- The Full device token never appears in a URL.
- A leaked media token reads that user's media for up to the TTL. It is not
  revoked with the device token or by signing out in this slice (follow-up);
  the TTL, capped at a day, is the bound.
- The stream prefixes admit everything under `/videos/{id}/` and
  `/audio/{id}/`, including subtitles, trickplay tiles, attachments and
  lyrics, which a native player also needs.
- Each client refreshes its token on a timer, and a stream that outlives the
  TTL needs the player to take a new URL.
- A new media route is closed to Media tokens until it is added to
  `MEDIA_ALLOWLIST`.

## Alternatives considered

- Device token in `api_key`: a Full credential in URLs, history and proxy logs.
- Set the `pharos_media` cookie in the WebView: Android WebView blocks
  third-party cookies, and setting one needs JNI `CookieManager`.
- Proxy every image and segment through the app's own custom protocol adding an
  `Authorization` header: every 4K segment would cross JNI, at an unmeasured
  cost.
- HMAC-signed per-URL links: HLS child URIs would need re-signing.

## References

- ADR-0034 (native UI media cookie), ADR-0042 (token scopes)
- B542, V437
