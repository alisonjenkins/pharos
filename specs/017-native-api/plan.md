# Implementation Plan: pharos native API (Cap'n Proto)

**Branch**: `017-native-api` (work lands on feature branches off `main`) | **Date**: 2026-11-01 | **Spec**: [spec.md](./spec.md)
**Input**: `specs/017-native-api/spec.md` (clarified 2026-11-01, 5 answers)

## Summary

Add a schema-first Cap'n Proto RPC API at `/pharos/rpc` (binary WebSocket) for
pharos-ui and, later, pharos's own phone/TV apps. The Jellyfin API stays
untouched for third-party clients. Both surfaces become thin wire layers over
shared domain services extracted from today's Jellyfin handlers, one behaviour
at a time (V172). Every RPC method is observed at HTTP parity through a
generated instrumentation wrapper (V173). A platform-neutral Rust client core
(`pharos-client`) is shared by the web UI now and the apps later.

Research verified the riskiest assumptions before design. capnp-rpc 0.27 builds
for wasm32 at ≤112 KB gzip (R1). Its `!Send` model fits actix's per-worker
runtime (R2). The initial schema ([contracts/pharos.capnp](./contracts/pharos.capnp))
compiles with `capnp` 1.5 and through `capnpc` 0.27 to Rust that builds for wasm32.

## Technical Context

**Language/Version**: Rust stable (workspace toolchain), wasm32-unknown-unknown for the client in the browser
**Primary Dependencies**: new: `capnp` 0.27, `capnp-rpc` 0.27, `capnp-futures` (transitive), `capnpc` 0.27 (build), `capnproto` 1.5 (devShell tool). Reused: `actix-ws` 0.4, `tokio-util` (compat), `tracing`, `metrics`, `opentelemetry`.
**Storage**: none new. Sessions use the existing hashed token store (ADR-0011).
**Testing**: `cargo nextest`. The in-process server and native `pharos-client` over TCP run in integration tests (`it_rpc` group); the tests bind the server's RPC dispatch to a test-only TCP listener, since v1 ships only the WebSocket (FR-002). Generated-wrapper tests cover V173. The schema-compat test covers FR-009. Playwright covers the browser transport.
**Target Platform**: Linux server (k8s); browsers per 009 NFR-005
**Project Type**: Rust workspace (server + wasm client + shared crates)
**Performance Goals**: home/library/detail in ≤ 1 batch after connect (SC-001). Payload and decode gate set from measurement (SC-002, T040).
**Constraints**: Jellyfin compat suite green at every commit (SC-003). No `unwrap`/`expect` (V17). No `Mutex` on the request path (V18). Metric labels bounded and asserted distinct.
**Scale/Scope**: single household; tens of concurrent connections

## Constitution Check

| Gate | Status |
|---|---|
| I. Wire compatibility | PASS. No Jellyfin endpoint changes; SC-003 runs the compat suite on every commit; services are extracted under existing Jellyfin tests |
| II. Group sync | N/A here; `Group` interface arrives with 014 and carries the V20 protocol |
| III. Test-first + ODD | PASS. Every task pair is test → implementation. Signals are named in spec §Observability and asserted by T010/T011 before any method ships |
| IV. Never panics/leaks | PASS. Typed error unions carry causes (FR-007). `$secret` annotation and redaction test (V8). Limits are typed (FR-010) |
| V. Types over conventions, actors over locks | PASS. Typed id structs on the wire. `RpcSystem` per worker thread; shared state via existing actors (R2). Trait boundaries: `Transport`, services |
| Tech: single binary, Nix canonical | PASS. RPC is served by the same binary; `capnproto` is added to the devShell and the OCI build inputs |
| V16 (amended) / V172 / V173 / ADR-0028 | PASS by construction (R4, R5) |

No violations; Complexity Tracking is empty. Re-checked after Phase 1: unchanged.

**ADR to write when the feature lands** (CLAUDE.md): Cap'n Proto + capnp-rpc
as the native wire (new dependency that shapes the design), including R2's
threading model and R6's cookie flow.

## Project Structure

### Documentation

```text
specs/017-native-api/
├── spec.md  plan.md  research.md  data-model.md  quickstart.md
└── contracts/pharos.capnp     # initial schema; becomes crates/pharos-api/schema/pharos.capnp
```

### Source code

```text
crates/pharos-api/                 # NEW — schema + generated code, native + wasm
├── schema/pharos.capnp
├── schema/compat/main.bin         # last released schema, for FR-009
├── build.rs                       # capnpc + Instrumented<S> generator (R4)
└── src/{lib.rs, instrument.rs, labels.rs, redact.rs}
crates/pharos-client/              # NEW — platform-neutral client core (FR-001a)
└── src/{transport.rs, connection.rs, session.rs, cache.rs, events.rs, instrument.rs}
    transport/{ws_web.rs (cfg wasm), tcp_native.rs (cfg not wasm)}
crates/pharos-server/src/
├── services/                      # NEW — shared domain services (V172, R5)
│   └── {library.rs, user_data.rs, playback.rs, events.rs}
├── api/pharos/rpc/                # NEW — native wire layer
│   └── {mod.rs (upgrade + RpcSystem), bootstrap.rs, session.rs, library.rs, user_data.rs, playback.rs, events.rs, telemetry.rs}
├── api/pharos/session_cookie.rs   # NEW — POST /pharos/session (R6)
└── api/jellyfin/items.rs …        # handlers rewired onto services/, behaviour unchanged
crates/pharos-server/tests/it_rpc/ # NEW integration group
```

`pharos-api` and `pharos-client` are added to `[final-excludes]` in
`.config/hakari.toml` for the wasm build, as `pharos-ui` is (ADR-0012).

## Phases (task outline for `/speckit-tasks`)

1. **Toolchain and measurement**: `capnproto` in the devShell and OCI inputs. Re-take the R8 numbers in the devShell (T001).
2. **Schema crate + instrumentation first**: `pharos-api` with the generated `Instrumented<S>`. The tests from spec §Observability (failing-handler span/counter/log, label distinctness, redaction, zero-registration) go red, then green (T010–T013).
3. **Transport + bootstrap**: `/pharos/rpc` upgrade, Origin allow-list, `POST /pharos/session` cookie, `Bootstrap.resume`, `Session.me`. Connection lifecycle metrics and logs. Native TCP transport in tests.
4. **Services extraction, one behaviour per commit**: views, latest, resume, next up, query, detail, user data, then playback negotiate/report. Each commit: the Jellyfin tests stay green, the service gets its own unit test, and the native method gets an `it_rpc` test.
5. **Events**: the services publish to the existing fan-out; `Events.subscribe` with seq, lag and drop metrics. Multi-replica uses ADR-0016's distribution.
6. **`pharos-client`**: connection, resume, reconnect + resubscribe, instrumentation, query cache primitives. The browser transport is tested in Playwright.
7. **Tooling**: `pharos rpc call` / `pharos rpc tap`, decoded debug logging, schema-compat test.
8. **Measurement gate**: SC-002 payload/decode benchmark vs Jellyfin JSON on the seeded library (T040). Set the gate and add the CI benchmark.
9. **Docs**: `docs/observability.md` named queries, `docs/jellyfin-mapping.md` native/Jellyfin mapping, the ADR.
