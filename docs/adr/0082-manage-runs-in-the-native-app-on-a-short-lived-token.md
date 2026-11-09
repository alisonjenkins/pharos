# ADR-0082: Manage runs in the native app on a short-lived admin token

- **Status:** Accepted
- **Date:** 2026-11-09T09:30:00Z
- **Deciders:** Alison Jenkins

## Context

The native web UI signs a browser in with an `HttpOnly` session cookie
(ADR-0029), so script never holds a token. Its Manage pages talk to the admin
endpoints of the Jellyfin API (`/Users`, `/Auth/Keys`, `/System/Configuration`,
`/Admin/Integrity/…` and the rest), which take a header token; no cookie may
authorize a write or a JSON read there (V191). The native API's `Admin`
capability is a stub with one method.

So `/manage` mounted the older prototype app (`RootApp`) instead: its own
banner and button row, its own Jellyfin sign-in, unstyled lists, and it
stopped playback (spec 020 FR-009). Alison called it "very ugly" (2026-11-09).

Porting every admin endpoint to the native API is a large server project with
no user-visible gain over reusing the REST endpoints the pages already call.

## Decision

We add `Session.manageToken` to the native API (ordinal 15, append-only). It
answers an admin with a Full-scope token, and anyone else `notPermitted`. The
token is issued with `issue_child_with_ttl` as a child of the session's own
token under the device id `pharos-native:manage`, so signing out, revoking the
device or deleting the user ends it in the same statement (V438), and it lives
`MANAGE_TOKEN_TTL_SECS` (one hour) at most, never past its parent.

`/manage` becomes a screen of the native app (`native::manage::ManageScreen`)
in the settings frame. It asks for a token on arrival and again five minutes
before each one ends, and passes it to the existing admin pane as
`X-Emby-Token`. The app keeps one token per connection and reuses it when
Manage opens again; a refused renewal keeps the held token until it ends. Every ask is counted by
`pharos_manage_token_issued_total{outcome=ok|not_admin|store_unavailable|rejected}`
and logged (`native manage token issued` / `refused`) with the token id
prefix, never the secret.

With the native API turned off there is no session to ask, so `/manage` still
mounts `RootApp` with its own sign-in.

## Consequences

- Manage looks like the rest of the app, keeps the header, theme and backdrop,
  and no longer ends playback: the stage shrinks to its compact view as on any
  other screen (spec 020 FR-009 is retired).
- Script on an admin's Manage page can read a Full token for up to an hour.
  This is not new reach: the same script can already drive the session's
  WebSocket, and an admin's rights include issuing API keys. It does add a
  token that can be carried off the page, so the lifetime is short and bound
  to the session. The older app held a 90-day Full token in script.
- A scope limited to admin routes was considered and not taken: an admin
  token that can create API keys and admins is Full in effect, so the
  allowlist would add upkeep without narrowing anything.
- Manage holds one token row at a time per connection; it expires within the
  hour like a media token, and `/Devices` hides it with the media tokens.
- The token is not revoked when the admin leaves Manage: it stays in script
  memory until it ends, at most an hour. Revoking it on leave needs a revoke
  call the native API does not have yet.

## Alternatives considered

- **Native admin API.** Port users, keys, branding, libraries, devices,
  activity, tasks, plugins, logs, duplicates and integrity to capnp methods.
  Right in the long run; weeks of server work for the same pages.
- **Keep `RootApp`, restyle it.** Still a second sign-in and a second app.
- **Accept the cookie on admin endpoints.** Breaks V191: a sibling subdomain's
  page could make the browser send it with a forged write.

## References

- B920, V880 (`specs/001-pharos-baseline/`); B921 for the redesign.
- ADR-0029 (session cookie), ADR-0034 (`pharos_media`), ADR-0058 (media token),
  ADR-0052 (one player on a stage).
- `crates/pharos-server/src/api/pharos/rpc/session.rs` (`manage_token`),
  `crates/pharos-ui/src/native/manage.rs`.
