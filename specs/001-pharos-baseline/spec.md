# Feature Specification: pharos baseline

**Feature Branch**: `main` (pre-existing system, retro-specified)
**Created**: 2026-07-27 (migrated from `SPEC.md` §G/§I/§V)
**Status**: Live in production; 81 of 100 tasks complete, 19 open (see `tasks.md`)

This is the baseline specification for pharos as a whole, not a single increment.
It was migrated from the cavekit `SPEC.md` when the project moved to spec-kit.
Sibling artifacts: `plan.md` (stack + architecture), `tasks.md` (T1…T100),
`invariants.md` (V1…V81, normative), `bugs.md` (B1…B129).

## Goal

A Rust media server, wire-compatible with Jellyfin clients (Phase 1) and Plex
clients (Phase 2), with better performance and reliability than both. Group watch
and group listen are first-class, not bolt-on.

Group sync is a **primary motivation**: Jellyfin's SyncPlay is buggy in practice —
late-joiner desync, one poor-network member dragging the whole group, buffer storms
on leader handoff. pharos must improve on those failure modes, not replicate them.

**Phase 1 priority order**: (1) Jellyfin client compat, (2) Dioxus web UI, (3) core
(scan / store / group-sync). Plex (T11–T14) waits until Phase 1 is solid.

## User Scenarios & Testing

### User Story 1 - Play from an unmodified Jellyfin client (Priority: P1)

A user opens jellyfin-web, the Android/Google TV app, Finamp or Infuse, points it at
pharos, signs in, browses their library and plays a title — direct play where the
client can take the source verbatim, transcoded where it cannot.

**Acceptance**: the client is not patched or configured specially. Covered live by
`tests/client_compat.rs` (Layer B, every PR) and by `just compat-playwright-full`,
which drives unmodified jellyfin-web headless against a real seeded library.

### User Story 2 - Watch together (Priority: P1)

Several members, on different devices and networks, join a SyncPlay group and watch
one title in lockstep. Play, pause and seek propagate; a member who buffers does not
strand the others indefinitely; a late joiner lands at the group's position; a
rolling deploy of the server does not end the party.

**Acceptance**: `just compat-syncplay` drives three real browsers through group-watch
scenarios; V19/V21/V25/V27/V28/V31/V65 encode the specific failure modes.

### User Story 3 - Browse and administer through the built-in UI (Priority: P2)

A user reaches pharos's own Dioxus web UI for browsing, playback and administration
(users, libraries, devices, activity, scheduled tasks, API keys, branding) without
needing jellyfin-web at all.

**Acceptance**: SSR coverage in `tests/ssr_render.rs` across every view, plus the
real-Chromium Playwright spec under `compat-playwright/`.

### Edge Cases

The bug ledger (`bugs.md`, 121 entries) is the authoritative catalogue of edge cases
this system has actually hit. Recurring classes:

- **Client dialect divergence** — camelCase query params, dashed vs dashless ids,
  fields a strict Kotlin SDK requires, two legal spellings of one request (V41, V38,
  V39, V69).
- **Source media that misbehaves** — moov-at-EOF mp4, HEVC/Dolby Vision, av1-in-mkv,
  10-bit sources, zero-byte files from failed copies (V6, V37, V40, V48, V56, V57).
- **Segment-grid arithmetic** — frame snapping, timescale, seek bias, audio anchored
  to the video grid; errors here surface as A/V drift or a stutter at every boundary
  (V54, V60, V67, V70, V71, V72, V78, V80).
- **Storage and scan faults** — a partial NFS listing that would sweep the catalog,
  concurrent refresh requests, background IO starving live playback (V5, V34, V51,
  V52, V58).
- **SyncPlay state edges** — stale queue-entry reports, a dropped socket holding a
  readiness gate, a member freezing the group and leaving (V27, V31, V65).

## Requirements

### Functional Requirements

Grouped by the §I interface surfaces they belong to:

- **FR-001 jellyfin-api** — HTTP/REST surface matching Jellyfin schemas: auth,
  system, library, items, playback, sessions.
- **FR-002 plex-api** *(Phase 2)* — HTTP/XML+JSON surface matching Plex schemas:
  auth, library, hubs, streaming, sessions.
- **FR-003 group-sync** — websocket protocol for synced playback across clients in a
  shared session, Jellyfin SyncPlay-shaped on the canonical socket.
- **FR-004 media-fs** — filesystem scan over configured roots, watching for changes.
- **FR-005 ffmpeg** — transcode, probe and thumbnail extraction.
- **FR-006 store** — sqlite (default) or postgres for metadata, users and sessions.
- **FR-007 config** — TOML file plus env override; path via `--config` or
  `PHAROS_CONFIG`.
- **FR-008 cli** — `pharos serve`, `pharos scan`, `pharos admin <subcommand>`.
- **FR-009 dioxus-ui** — Dioxus web frontend served by the backend as a WASM bundle,
  filling the jellyfin-web role.
- **FR-010 obs** — OpenTelemetry traces via OTLP, Prometheus metrics at `/metrics`,
  structured logs via `tracing`.
- **FR-011 health-api** — `/healthz` liveness, `/readyz` readiness, `/info`
  build + version.
- **FR-012 nix-flake** — `nix develop` devShell, `nix build .#pharos` server,
  `nix build .#oci` OCI image via `dockerTools`.

Behavioural requirements are stated as invariants rather than prose: see
`invariants.md` (V1…V81). Those are normative and are what plans are checked
against.

## Success Criteria

### Measurable Outcomes

- **SC-001** — An unmodified Jellyfin client connects, browses and plays both direct
  and transcoded media (V1).
- **SC-002** — Group session play/pause/seek syncs across members within 500 ms p95
  (V3).
- **SC-003** — Shared fields of API responses are byte-equivalent to the reference
  Jellyfin schema (V7).
- **SC-004** — Library scan never blocks playback or API requests (V5), and never
  erases the catalog from a partial source listing (V51).
- **SC-005** — An ffmpeg or libav fault never takes the server down (V6).
- **SC-006** — A SyncPlay party survives a rolling deploy end to end with no silently
  dropped command (V25).
- **SC-007** — Every deployed behaviour change is confirmed by running its named
  LogQL/PromQL query against production, not by assertion.

## Assumptions

- The deployment is a home k3s cluster behind Flux, currently `replicaCount: 1` with
  a Postgres (CNPG) store; multi-replica correctness work (T85, T91) is deliberately
  held until that changes.
- Jellyfin remains the reference implementation for wire shape; where its docs and
  its actual client behaviour disagree, client behaviour wins.
- Plex compatibility is genuinely deferred, not abandoned; T11–T14 hold their ids.
