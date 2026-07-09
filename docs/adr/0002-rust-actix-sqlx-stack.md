# ADR-0002: Rust / actix-web / sqlx / tokio stack

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

A media server is a long-running network service that muxes untrusted input
(arbitrary media files), spawns/embeds ffmpeg, streams large responses, and must
stay up unattended on home infrastructure. It needs: strong memory safety around
FFI (libav), good async IO for streaming, and a single static-ish binary that
ships cleanly in a container.

## Decision

We build pharos in **Rust**, as a Cargo **workspace** of focused crates
(`pharos-core`, `pharos-server`, `pharos-transcode`, `pharos-cache`,
`pharos-scanner`, `pharos-store-sqlx`, `pharos-jellyfin-api`, `pharos-ui`).

- **HTTP:** `actix-web` — mature, fast, first-class streaming bodies (needed for
  HLS segment + progressive transcode responses) and an extractor model that
  makes the auth boundary explicit (`AuthUser`).
- **Async runtime:** `tokio`.
- **DB access:** `sqlx` — compile-time-checked queries, async, backend-portable
  (see ADR-0003).
- **CLI:** `clap` (derive).
- **Observability:** `tracing` + `metrics` + Prometheus (see
  `docs/observability.md`).

Clippy runs with `unwrap_used` / `expect_used` denied (invariant V17) outside
tests, forcing explicit error propagation.

## Consequences

- Memory safety across the libav FFI boundary is a language guarantee, not a
  review burden — important given ADR-0004 runs libav in-process.
- The workspace split keeps `pharos-core` free of IO/framework deps (domain
  types only), so the store, API, and transcode layers depend inward.
- `sqlx`'s compile-time query checking requires a reachable schema at build
  time (offline query cache) — a build-ergonomics cost.
- The `unwrap`/`expect` deny lint means more `Result` plumbing but no panics in
  request paths.
- Rust build times are non-trivial; mitigated by `workspace-hack` (cargo-hakari)
  and the pinned toolchain (ADR-0009).

## References

- `docs/architecture.md`, `docs/structure-eval.md`
- `Cargo.toml` (workspace), `CLAUDE.md` (§Stack)
