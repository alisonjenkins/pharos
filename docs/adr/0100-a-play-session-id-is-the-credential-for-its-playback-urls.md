# ADR-0100: A play session id is the credential for its playback's URLs

- **Status:** Accepted
- **Date:** 2026-11-09T23:20:00Z
- **Deciders:** Alison (security priority set in the 2026-11-09 spec audit), Claude

## Context

pharos is reachable from the internet. The Jellyfin `PlaybackInfo` answer put
the caller's session token into the `TranscodingUrl` as `api_key`, every HLS
playlist copied it into each URL it listed, and subtitle and font
`DeliveryUrl`s carried it as well. Those URLs land in reverse-proxy access
logs, player logs, crash reports and screenshots. The token behind them signs
in as the user until it is revoked.

Header-less players need some credential in the URL: ExoPlayer on the Jellyfin
Android apps has no auth interceptor, and a `<video src>` cannot send a header.
Since B75 the PlaySessionId (a random uuid that `PlaybackInfo` registers against
one item and the token that asked) already authorizes a tokenless
`/stream?tag=`, and since B256 each use re-checks that the holder's token is
live and that they may still see the item, and the session ends at stop or six
hours after negotiation.

## Decision

We will use the PlaySessionId as the only credential in the URLs pharos builds
for a playback.

- `play_capability::PlaybackUser` authenticates the HLS playlist and segment
  routes and `/audio/{id}/universal`: a token (header, `api_key`, cookie) first,
  then a live PlaySessionId for the path's `{id}`.
- `require_bearer` (subtitle and font files) accepts the same capability.
- `PlaybackInfo` builds the `TranscodingUrl` and the `DeliveryUrl`s with the
  PlaySessionId and no `api_key`.
- A playlist request whose PlaySessionId is that user's live session for the
  item embeds no token in the URLs it lists. One without a live session still
  copies the token it was given, so a client that builds its own master URL
  keeps working.

## Consequences

- A leaked playback URL exposes one item for at most the life of one playback,
  not the account.
- A paused playback resumed more than six hours after `PlaybackInfo`, or after
  the user's 32-session cap evicted it, gets a 401 on its next segment. A header-less
  Jellyfin client then has to negotiate again, as one direct-playing through
  `/stream?tag=` already does. Clients that send a header or the auth cookie
  (jellyfin-web, the pharos UI) are not affected.
- Each capability-authenticated request reads the stored session row (B256), as
  `/stream?tag=` already does.
- pharos-ui's legacy `views/` player still builds `api_key` URLs on the client.
  It is outside this change.

## Alternatives considered

- **Signed, short-lived URLs (HMAC over path and expiry).** Needs a server key
  and rotation, and every playlist would have to re-sign its URLs. The
  PlaySessionId already has an owner, an item scope, an end and revocation.
- **A separate per-playback media token.** The same properties as the
  PlaySessionId, with a second id to keep in step.
- **Leaving `api_key` in and redacting logs.** pharos already redacts its own
  logs. That does not reach the proxy, the player or a screenshot.

## References

- `crates/pharos-server/src/api/jellyfin/play_capability.rs`
- 001 `bugs.md` B75, B256, B1090; `invariants.md` V189, V1050
- 017 `tasks.md` T064
