# Architecture Decision Records

This directory records the **load-bearing decisions** behind pharos — the
choices that shaped the architecture and would be expensive or contentious to
reverse. Each record captures the context at the time, the decision, and its
consequences, so a future reader (or the person who made it) can reconstruct
*why* without archaeology through git history.

## Format

Lightweight [Nygard-style](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
ADRs. One file per decision, `NNNN-kebab-title.md`, monotonically numbered.
Copy [`0000-template.md`](0000-template.md) to start a new one.

Sections: **Status**, **Context**, **Decision**, **Consequences** (and
optional **Alternatives considered** / **References**).

## Status values

- **Accepted** — in force.
- **Superseded by ADR-NNNN** — replaced; kept for history (never delete an ADR).
- **Proposed** — under discussion, not yet in force.
- **Deprecated** — no longer relevant, but not actively replaced.

## Conventions

- ADRs are **immutable once Accepted**. To change a decision, write a new ADR
  that supersedes the old one and update both `Status` lines. This preserves
  the decision timeline.
- Keep them short. An ADR is the *why*, not a design doc — link to `SPEC.md`,
  `docs/architecture.md`, or code for the *how*.
- Dates are ISO-8601 UTC.

> **Backfill note.** ADRs 0001–00xx were written retroactively (2026-07) to
> capture decisions already embodied in the codebase, so pharos has a reviewable
> decision log going forward. Their dates reflect *when documented*, not
> necessarily when first decided; where the original decision date is known it
> is noted in-text.

## Index

| # | Title | Status |
|---|-------|--------|
| [0001](0001-jellyfin-api-compatibility.md) | Jellyfin API as the client contract | Accepted |
| [0002](0002-rust-actix-sqlx-stack.md) | Rust / actix-web / sqlx / tokio stack | Accepted |
| [0003](0003-sqlite-default-store.md) | SQLite (WAL) default store, Postgres alternative | Accepted |
| [0004](0004-ffmpeg-libav-default-backend.md) | libav in-process default backend + crash-isolated worker pool | Accepted |
| [0005](0005-per-segment-hls-transcode.md) | Per-segment on-demand HLS transcode + VP9-in-fMP4 for Firefox | Accepted |
| [0006](0006-subtitles-out-of-band.md) | Subtitles delivered out-of-band, never muxed | Accepted |
| [0007](0007-trickplay-pregeneration.md) | Trickplay pre-generation with playback-yield gate | Accepted |
| [0008](0008-incremental-scan-signature.md) | Incremental scan by (mtime, size) signature | Accepted |
| [0009](0009-nix-flake-reproducibility.md) | Nix flake + devShell; buildRustPackage for the OCI image | Accepted |
| [0010](0010-cd-ghcr-flux-automation.md) | CI on self-hosted builder → GHCR → Flux image automation | Accepted |
| [0011](0011-auth-hashed-bounded-tokens.md) | Hashed, bounded-lifetime session tokens + CLI admin bootstrap | Accepted |
| [0012](0012-dioxus-wasm-ui.md) | Dioxus + dx (WASM) for the built-in web UI | Accepted |
| [0013](0013-observability-tracing-metrics.md) | Structured tracing + Prometheus metrics + OTLP traces | Accepted |
| [0014](0014-watch-together-group-sync.md) | Watch-together group-sync over a WebSocket, Jellyfin SyncPlay-compatible | Accepted |
