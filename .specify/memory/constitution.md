# pharos Constitution

Migrated from `SPEC.md` §C (constraints) and §V (invariants) on 2026-07-27, when
the project moved from the cavekit `/ck:*` workflow to GitHub spec-kit.

## Core Principles

### I. Wire compatibility is the product (NON-NEGOTIABLE)

Unmodified third-party clients are the acceptance test. A Jellyfin client
(jellyfin-web, Android/Google TV, Finamp, Infuse) must connect, browse and play —
direct and transcoded — with no patching (V1). Response shapes match the reference
schemas byte-equivalent for shared fields (V7), and every wire object a strict SDK
deserializes is emitted from a typed DTO, never a `serde_json::json!` literal
(V38), with enum-typed fields restricted to real enum members (V39). Where a
client dialect has two legal spellings of one request, pharos serves both (V69).

Phase 1 is Jellyfin compat + Dioxus UI + core (scan/store/group-sync), in that
priority order. Plex compat (T11–T14) is Phase 2 and blocks nothing.

### II. Group sync must beat Jellyfin's, not replicate it

Group watch/listen is a primary motivation, not a bolt-on. The wire protocol stays
Jellyfin SyncPlay-compatible so stock clients participate; the improvements live in
the server algorithm (V20). Concretely (V19): a late joiner does not desync
existing members; one poor-network member does not stall the group; leader handoff
costs at most one corrective Pause; a sub-2 s blip reconverges without rejoin.
Play/pause/seek sync within 500 ms p95 (V3). The server never withholds a command a
client must ACK (V21), freezes are always bounded and always resume (V31), and a
party survives a rolling deploy end to end (V25).

### III. Test-first, then prove it by query (NON-NEGOTIABLE)

TDD: a failing test precedes the implementation change; no "test later" (V11). No
merge without failing-test-first evidence. Domain logic sits behind traits and is
testable with no DB, filesystem, network or ffmpeg (V12).

Observability-driven development runs alongside, not instead of. Before writing a
fix, name the exact LogQL/PromQL query that shows the bug happening now; if none
exists, the first commit adds the instrumentation, separately, so it can ship and
be read before the fix lands. Instrument decisions, not just errors: any branch
choosing between behaviours records its inputs, its verdict and the reason —
carrying the offending value, never a bare class. Whatever the success path
records, the failure path records too. Assert the signal in a test and confirm the
test fails without the instrumentation. After deploying, run the named query and
report its output; "should be fixed" is not a result.

### IV. It never panics, never leaks, never lies

No `unwrap()`/`expect()` in non-test code, enforced by `clippy::unwrap_used` +
`clippy::expect_used` = deny at workspace level (V17). An HTTP handler never panics;
errors return a structured response in the target API's schema (V4). An ffmpeg
subprocess crash never crashes the server (V6) and a library scan never blocks
playback or API requests (V5). Auth tokens are never logged and secrets are redacted
everywhere (V8); media paths never reach an unauthenticated client and traversal is
blocked at the boundary (V9). Store writes are atomic per logical op — no partial
entry is ever visible to a reader (V10).

Errors expose their cause: never collapse a failure to a bare class; carry the
underlying value or message.

### V. Types over conventions, actors over locks

Make incorrect states unrepresentable — a settled decision gets its own type whose
invalid form cannot be constructed (V60, V68, V71). Mutable runtime state is owned
by exactly one task and mutated by message passing over tokio mpsc; no
`Mutex<State>` on the request path, locks only for one-shot init or
immutable-after-init caches (V18). Trait boundaries sit at every IO edge; call sites
depend on traits, so backends swap by wiring, not refactor.

## Technology Constraints

- Rust stable, backend and frontend. actix-web (perf over axum). clap derive, no
  builder API. Dioxus → WASM for the web UI, which consumes only the public
  Jellyfin-compat API — no backdoor endpoints (V16).
- sqlx behind `pharos-core` traits (`MediaStore` et al.); sqlite default, postgres
  feature-gated. Native `async fn` in traits, no `async_trait`; generics over
  `dyn Trait` for swappable backends.
- SIMD-accelerated crates where applicable: `sonic-rs` (JSON), `xxhash-rust` xxh3
  (stable ids), `blake3` (content hashing), `image` (decode). Graceful fallback on
  unsupported archs. Every hot or SIMD path carries a `criterion` bench.
- ffmpeg for transcode/probe/thumbnail. Two interchangeable backends by Cargo
  feature: `ffmpeg-lib` (default, in-process libav via a crash-isolated worker pool)
  and `ffmpeg-spawn`. libav diagnostics never reach stderr — they route through the
  `av_log` bridge onto `tracing` (V59).
- Single-binary deploy. TOML config + env override. No runtime deps beyond ffmpeg.
- Nix flake is canonical: `nix develop` for the shell, `nix build .#pharos` for the
  server, `nix build .#oci` for the image via `dockerTools`. No `Dockerfile`. CI uses
  the same flake.
- Observability baked in from T1, never retrofit: OTLP traces, Prometheus `/metrics`,
  structured logs via `tracing` only — no `println!`/`eprintln!` in non-test code
  (V15). Every inbound request traced, every outbound IO spanned, `trace_id` in logs
  (V13). `/healthz`, `/readyz`, `/metrics` per V14. Metric labels are a dashboard
  contract: bounded cardinality, stable strings from a `label()` method, asserted
  distinct in a test.

## Development Workflow

- Work inside the Nix devShell; host toolchains drift from CI.
- `cargo nextest run --workspace` is the runner; doctests via `cargo test --doc`.
  Iterate with `just test-fast` / `just test-changed`, run the full `just test`
  before a commit, and `just test-postgres` after touching any `sqlx::query*` string.
- Atomic commits: one logical change each, revertable alone. Never squash.
- Design by mining Jellyfin's structure and feature list for ideas, then adapting
  idiomatically — traits at IO boundaries, not a class-for-class translation.

## Governance

This constitution supersedes other practice. The normative teeth are the invariant
register at `specs/001-pharos-baseline/invariants.md` (V1…V81) — every plan is
checked against it, and every bug in `specs/001-pharos-baseline/bugs.md` names the
invariant that now guards its class.

Invariant, task and bug ids are stable and load-bearing: they are cited by each
other, by tests and by code comments. Never renumber; always append. A new invariant
is added when a bug reveals a property that was assumed but unenforced.

`SPEC.md` at the repo root is the frozen pre-migration archive. It is history — do
not mutate it. All new spec work goes through the `/speckit-*` skills.

**Version**: 1.0.0 | **Ratified**: 2026-07-27 | **Last Amended**: 2026-07-27
