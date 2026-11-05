# ADR-0059: Media tokens are children of the issuing session token

- **Status:** Accepted
- **Date:** 2026-11-05T01:40:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0058 introduced the Media-scoped token from `Session.mediaToken`, stored
as its own `auth_tokens` row, and accepted as a consequence that it is not
revoked with the device token or by signing out: the TTL (at most a day) was
the only bound. On a public server that leaves a leaked stream URL working
after the user signed out, revoked the device, or changed their password
(B543). A disabled user was already refused on every lookup, and deleting a
user already cascaded to their tokens.

## Decision

We will store a Media token as a child of the session token that issued it.

- Migration 0065 adds `auth_tokens.parent_hash`, nullable, referencing
  `auth_tokens(token_hash)` with `ON DELETE CASCADE`, and an index on it.
  Deleting the parent row, by any path, deletes its children in the same
  statement. `Session.mediaToken` issues through
  `TokenStore::issue_child_with_ttl`; a Media token is never issued without a
  parent.
- A child's `expires_at` is the earlier of now plus the requested TTL and its
  parent's `expires_at`, and issuing against a missing or expired parent
  fails with `InvalidToken`. The expiry the store returns is the one
  `Session.mediaToken` reports.
- Revokes that go through `services::auth` count the children they are about
  to take in `pharos_media_token_revoked_total{cause}` (the `RevokeCause`
  label) and log it, using `TokenStore::count_live_children`.

## Consequences

- Sign-out, device revoke, session sweep, revoke by id and user delete end the
  Media token at once, and so does any revoke path added later.
- The count is a separate query run just before the revoke, so it can drift
  from the revoke's own filters or race with a concurrent issue. It is
  informational only: the removal is the cascade and never depends on it.
- Revokes that bypass `services::auth` (the Jellyfin session delete, cast
  hydrate) still cascade but are not counted.
- A password change keeps the caller's session token but, because the sweep
  also matches the Media row, drops that session's Media token; the client
  asks for a new one.
- Issuing needs the parent row to exist, so a session revoked a moment before
  the call gets a refusal rather than a token.

## Alternatives considered

- Look up the parent on every request: an extra query per image or segment,
  on the hottest path, and each new credential check must remember to do it.
- Delete the children explicitly in each revoke path: every future revoke
  path must remember to, and one that forgets reopens the hole silently.

## References

- ADR-0058; B543, V438 in `specs/001-pharos-baseline/`.
- `crates/pharos-store-sqlx/migrations/*/0065_auth_token_parent.sql`
