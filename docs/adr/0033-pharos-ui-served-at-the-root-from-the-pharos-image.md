# ADR-0033: pharos-ui is served at the root, from the pharos image

- **Status:** Superseded by ADR-0038
- **Date:** 2026-11-02T07:40:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0012 served pharos-ui under `/ui/*`. It could come from either of two places: an angie sidecar (the chart's `pharosUi`) that proxies REST to pharos, or pharos itself via `[server].ui_dir`. In practice:

- CI never published the sidecar image (`pharos-ui`), so the cluster could not run it.
- The `pharos` image carried no bundle, so `ui_dir` had nothing to point at.

Spec 009 moves the UI to history-mode routes at `/` (`/browse`, `/title`, `/watch`, …), and spec 017 opens the native API on the same origin at `/pharos/rpc`. A sidecar would also have to proxy the WebSocket upgrade and keep its idle timeout below the ingress's. That adds a second place where origin checks and cookies can go wrong.

## Decision

We will build the release bundle (`pharosUiBundle`, the `dx build` output) into the `pharos` OCI image at `/share/pharos-ui`. pharos serves it itself when `[server].ui_dir` is set. The chart defaults `config.server.uiDir` to that path.

pharos serves the bundle at `/` (`crates/pharos-server/src/ui.rs`, V204):

- **The UI is registered before every other route.** Its default service sees only paths that no API route claimed.
- **SPA fallback.** A GET that asks for `text/html` and does not name a file gets `index.html` with `no-cache`.
- **Files and caching.** A file in the bundle is served from an index built at startup. Paths are matched lowercased, because `LowercasePath` folds them. Anything under `assets/` is served `public, max-age=31536000, immutable`.
- **No HTML for API clients.** Everything else gets 404. An API client, a non-GET or a missing file never receives HTML.
- **The old mount moves.** `/ui` and `/ui/<rest>` answer `308` to `/<rest>`.
- **UI routes are reserved.** The first segments the UI routes are listed in `ui::UI_ROUTES`. A test sends each one through the whole API router.
- **Root fallback when off.** With `ui_dir` unset, `/` still answers `200 "pharos"`.
- **Bad bundles stop startup.** A bundle with no `index.html` fails at startup with an error that names the directory.

This supersedes the "build wiring" serving paragraph of ADR-0012, which is otherwise unchanged. The `pharosUi` sidecar defaults to disabled.

## Consequences

- One image and one origin serve the UI, the Jellyfin API and the native API, so the WebSocket needs no proxy hop of its own.
- The UI ships and rolls back with the server, so they can never be on different versions.
- The image grows by the bundle's size (a few MB). The `oci` CI job now also runs `dx build`, which lengthens it.
- A configured `ui_dir` without a bundle stops startup instead of serving the API without a UI. An image built without the bundle must clear `uiDir`.
- The CSP's `connect-src 'self'` means the UI talks only to the server that served it. The old server picker could switch to another server's URL, and it no longer can. A UI for a different server is that server's own UI.
- Only content-hashed assets (`-dxh<hex>` in the name) are immutable. Unhashed files such as `assets/pharos.css` revalidate.
- UI routes and API routes share one namespace. The collision test catches a new API route that would shadow a UI route. Because Jellyfin routes are matched case-insensitively, a UI route must never take a Jellyfin path's name.

## Alternatives considered

- **Publish the sidecar image.** It is less work in the image, but it needs a new GHCR package with repository access, a second rollout, and WebSocket proxying in angie for `/pharos/rpc`.
- **Keep `/ui/`.** Spec 009 needs bookmarkable root URLs (FR-002). The `/ui` prefix would also have to be stripped and re-added by the router.

## References

- `specs/009-ui-foundation/contracts/serving.md`
- `crates/pharos-server/src/ui.rs`
- `crates/pharos-server/tests/it_wire/ui_serving.rs`
- `flake.nix` (`ociImage`, `pharosUiBundle`)
- ADR-0012
- ADR-0029
