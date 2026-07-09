# ADR-0012: Dioxus + dx (WASM) for the built-in web UI

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos's primary client is unmodified jellyfin-web (ADR-0001) — that is what
makes existing phones and TVs "just work", and it stays the compat baseline.
But a bundled third-party JS SPA is opaque to the server: it cannot share the
Rust DTOs, it drags in a Node/npm toolchain, and every pharos-specific affordance
has to be smuggled through Jellyfin-shaped extensions or forked into jellyfin-web.

We wanted a *first-party* UI we own end to end — one that reuses the same request
and response types the server already defines, needs no JavaScript build stack,
and can grow pharos-native features (e.g. the richer group-sync path of
ADR-0014) without fighting Jellyfin's data shapes.

## Decision

pharos ships an **optional** first-party web UI written in Rust with **Dioxus
0.7**, compiled to WebAssembly by the **`dx`** CLI and served under `/ui/*`. It
lives in `crates/pharos-ui`; the wasm renderer is behind a `web` Cargo feature
(`dioxus-web` + `gloo-net` HTTP client + `web-sys`) exposing the
`pharos-ui-web` bin target. Components are authored once and render both to the
browser (`dioxus-web`) and, in `#[test]`s, to HTML strings via `dioxus-ssr` for
structural assertions without a browser.

Build wiring:

- The `wasm32-unknown-unknown` target is pinned in `rust-toolchain.toml`, so
  `dx build --package pharos-ui --release` (and plain `cargo build --target
  wasm32-unknown-unknown`) work inside the devShell with no extra setup.
- `pharos-ui` deliberately does **not** depend on `workspace-hack` — the hack
  pulls the native-only stack (sqlx/tokio/mio) that will not build for wasm32;
  it is listed under `[final-excludes]` in `.config/hakari.toml` so CI's
  `hakari-check` does not try to re-add it.
- The bundle is served two ways: a separate angie sidecar (`pharosUi` in the
  Helm chart, reverse-proxying REST to pharos same-origin), or in-process via
  `[server].ui_dir`. jellyfin-web (`jellyfinWeb`) is deployed alongside it.

The UI is **optional and gated** by `pharosUi.enabled` in the chart, distinct
from `jellyfinWeb.enabled`; the two front-ends coexist, and jellyfin-web remains
the primary/compat client.

## Consequences

- The stack is Rust end to end: one language, shared serde types with the
  server, no npm/Node in the build, reproducible under the same Nix devShell.
- We trade a mature UI ecosystem for a young one. The Dioxus surface is far
  behind jellyfin-web — the parity audit puts it at ~15% of jellyfin-web's
  feature units — so pharos-ui is a complement, not yet a replacement.
- wasm builds are a second target to keep green (feature-gating, `getrandom`
  `js` backend, the workspace-hack exclusion) — a small ongoing tax on CI.

## References

- `crates/pharos-ui/` (`Cargo.toml`, `src/bin/web.rs`)
- `docs/dioxus-parity-audit.md`; `CLAUDE.md` §Web UI build
- `charts/pharos/values.yaml` (`pharosUi`, `jellyfinWeb`)
- ADR-0001 (Jellyfin API as the client contract)
