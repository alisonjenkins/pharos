# ADR-0041: Access tokens carry a scope; a TV's token is playback-only

- **Status:** Accepted
- **Date:** 2026-11-02T20:30:00Z
- **Deciders:** Alison Jenkins

## Context

The stock Chromecast receiver needs an access token in every command, and it
uses that token for its own REST calls (item, PlaybackInfo, streams, images,
playback reports). Before this change, every token authenticated as its user
with the user's whole policy, admin routes included (research.md, T005).
The token sent to a TV crosses the LAN inside a Cast session whose TLS
accepts any certificate, so it must not grant the owner's account.

## Decision

We will give every stored token a scope, `TokenScope { Full, Playback }`:

- A nullable `scope` column on `auth_tokens` (migration
  `0061_auth_token_scope`). Absent means `Full`, so every existing token and
  every login is unchanged.
- A `Playback` token reaches only a deny-by-default allowlist of the routes
  the receiver calls (`PLAYBACK_ALLOWLIST` in `auth_extractor.rs`). Any other
  route answers 403 and counts `pharos_token_scope_refused_total{scope}`.
- Every credential check that grants the whole account (the native API's
  resume, session calls and socket re-check) goes through
  `services::auth::user_for_token`, which refuses a restricted token. Routes
  that a restricted token may reach use `scoped_user_for_token` and check the
  matched route.
- A TV token is minted per cast, bound to the TV's device id, lives 12 hours
  by default, is renewed at half its lifetime, and is revoked on every exit
  from the cast (V225).

## Consequences

- A leaked TV token can play and report on media the owner may see, and
  nothing else.
- Every new route that a receiver needs must be added to the allowlist on
  purpose. A route that is not listed is refused, which is the safe failure
  but can break casting until it is added.
- `TokenScope` is a closed enum, matched exhaustively, so a new scope is a
  deliberate code change.

## Alternatives considered

- **Send the owner's own token.** Grants the whole account, admin included,
  to anything that can read the Cast session.
- **A denylist of admin routes.** A new route would be open to TV tokens by
  default.

## References

- `crates/pharos-core/src/auth.rs` (`TokenScope`),
  `crates/pharos-server/src/api/jellyfin/auth_extractor.rs`,
  `crates/pharos-server/src/services/auth.rs`
- Invariant V225; ADR-0040
