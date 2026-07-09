# ADR-0001: Jellyfin API as the client contract

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos is a personal media server whose consumers are existing, unmodified
media clients: jellyfin-web in a browser (Zen/Firefox), the Jellyfin
Android/Google TV app, and mobile apps. Writing bespoke clients for every
platform is out of scope for a one-person project; the girlfriend's TV needs to
"just work" with an app from the store.

The realistic options for a client contract are: invent a pharos-native API
(and build/maintain clients), or implement an existing widely-supported server
API so third-party clients treat pharos as a drop-in.

## Decision

We implement the **Jellyfin server API** as pharos's external contract. pharos
speaks enough of the Jellyfin REST surface (auth, `/Items` browsing, image and
subtitle delivery, HLS/progressive playback, QuickConnect, the dashboard admin
tree) that unmodified Jellyfin clients drive it. The `pharos-jellyfin-api` crate
owns the DTOs; `crates/pharos-server/src/api/jellyfin/` owns the handlers.
Route paths are registered lowercase behind a `LowercasePath` middleware that
folds Jellyfin's PascalCase.

Fidelity is enforced by tests, not vibes: `route_auth_audit.rs` asserts every
route's auth boundary, `tests/client_compat.rs` drives a real-device-shape flow
with strict serde DTOs, and `just compat-playwright-full` runs unmodified
jellyfin-web headless (see ADR-0010).

## Consequences

- Any client in the Jellyfin ecosystem works with zero pharos-specific code.
- pharos is bound to Jellyfin's data shapes and quirks — bugs are often
  "jellyfin-web expects field X in shape Y" (e.g. the `Trickplay` map must be
  double-nested by media-source id; text subtitles are fetched as `Stream.js`
  JSON, not `.vtt`). Discovering these requires auditing jellyfin-web behaviour.
- pharos-specific features must be expressed as compatible extensions (extra
  query params, extra fields) rather than new endpoint shapes, or they break
  clients.
- We inherit Jellyfin's auth model (bearer token via `X-Emby-Authorization`).

## References

- `docs/jellyfin-mapping.md`, `docs/jellyfin-parity-audit.md`
- `crates/pharos-jellyfin-api/`, `crates/pharos-server/src/api/jellyfin/`
