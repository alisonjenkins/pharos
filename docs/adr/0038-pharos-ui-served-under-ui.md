# ADR-0038: pharos-ui is served under `/ui/`, not at the root

- **Status:** Accepted
- **Date:** 2026-11-02T09:00:00Z
- **Deciders:** Alison Jenkins

Supersedes ADR-0033.

## Context

ADR-0033 mounted pharos-ui at `/` and reserved its route names (`ui::UI_ROUTES`) out of the API namespace. On the shared host, jellyfin-web is the default client and must stay at `/`. Two problems follow from owning the root:

- pharos-ui would share the route namespace with the Jellyfin API. Every UI route would be a first segment reserved against the whole Jellyfin API space, and a new Jellyfin route could shadow a UI route.
- Jellyfin routes are matched case-insensitively (the server lowercases paths), so a UI route can collide with a Jellyfin path by case alone.

## Decision

We will serve the bundle under `/ui/` (`crates/pharos-server/src/ui.rs`), with the rules ADR-0033 set for the root mount:

- `/ui/` serves `index.html`. The SPA fallback applies only to a GET asking for `text/html` on a path under `/ui/` that names no file.
- Hashed `assets/` are `public, max-age=31536000, immutable`; every other file is `no-cache`.
- An API client, a non-GET or a missing file gets 404, never HTML. Only files indexed at startup are read. A bad bundle fails startup.
- The CSP, `X-Frame-Options: DENY` and `nosniff` headers are unchanged.
- `/ui` answers `308` to `/ui/`, keeping the query.
- `/` answers the core root response whether or not a UI is configured. Paths outside `/ui/` get the normal server 404; the UI is no longer a root default service.
- `ui::UI_ROUTES` goes away. The only reserved first segment is `ui`.
- pharos-ui is built with `base_path = "ui"` and its client routes are prefixed with `/ui`. Native API calls (`/pharos/rpc`, `/pharos/session`) stay unprefixed.

## Consequences

- jellyfin-web keeps `/` on the shared host, and pharos-ui no longer competes for names in the Jellyfin API space.
- The old `/ui/<rest>` to `/<rest>` redirect goes. Root deep links such as `/browse/x` no longer serve the UI, so bookmarks made under ADR-0033 break.
- The UI and the API stay on one origin, so the CSP, the cookies and the WebSocket need no change.
- Hashed asset names and the bundle layout are unchanged; the server maps bundle-root files to `/ui/<file>`.

## Alternatives considered

- **`/pharos/`.** It collides with the native API at `/pharos/rpc` and `/pharos/session`.
- **Keep the root mount.** It displaces jellyfin-web as the default client and keeps the namespace sharing described above.

## References

- ADR-0033
- ADR-0012
- `crates/pharos-server/src/ui.rs`
- `crates/pharos-server/tests/it_wire/ui_serving.rs`
