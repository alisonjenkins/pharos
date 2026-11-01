# Research: 017-native-api

**Date**: 2026-11-01. R8 was re-taken in the devShell on 2026-11-01 (T001);
quickstart.md §0 holds the numbers.

## R1 — capnp-rpc compiles to wasm32 and is small enough

- **Decision**: use `capnp` 0.27 + `capnp-rpc` 0.27 + `capnpc` 0.27 (build-time)
  with the `capnp` 1.4.0 compiler from nixpkgs.
- **Evidence**: a spike crate (bootstrap → `resume()` → `library()` →
  `latest()`, pipelined in one flight over a generic `AsyncRead`/`AsyncWrite`)
  builds for `wasm32-unknown-unknown` with no feature changes. Stripped,
  `opt-level="z"`, LTO: **343 KB raw / 112 KB gzip**. That is an upper bound on
  the increment, because it includes std, alloc and panic machinery pharos-ui
  already links.
- **Rationale**: pipelining and callback capabilities (Clarifications Q1)
  need the RPC layer; the cost is about a fifth of today's UI bundle at most.
- **Alternatives**: protobuf + gRPC-web (needs a proxy, no pipelining), FlatBuffers
  (no RPC), JSON over WebSocket (the inefficiency being removed).

- **Finding (T001, 2026-11-01)**: capnpc 0.27 generates no pipeline accessor
  for a capability inside a union, and `Bootstrap.resume` returns
  `SessionResult`, a union of `session` and `error`. So `resume` has to be
  awaited before `session.library()` can be called, and the chain is two round
  trips, not the single flight claimed above and in US1. `library()` and
  `latest()`/`home()` do pipeline off the session capability. The wire protocol
  can pipeline through a union pointer; only the generated Rust lacks the
  accessor. **Decided 2026-11-01 (Alison, option a)**: `resume` returns
  `ResumeResult { session :Session; status :Done }`, plain fields, so
  `resume → session.library() → library.home()` is one flight and SC-001 / US1
  stand as written. A refused resume returns a session whose every call fails
  `Error.kind.revoked`, and `status.error` carries the reason; the generated
  wrapper reads the call's outcome from `status` when `result` is not a union.
  `login` and `quickConnectPoll` keep their unions: they run once per device,
  not on every page load.

## R2 — capnp-rpc is `!Send`; actix fits it

- **Finding**: `RpcSystem` and capability clients use `Rc` internally, so they
  cannot cross threads.
- **Decision**: each `/pharos/rpc` connection runs its `RpcSystem` on the actix
  worker thread that accepted the upgrade, via `actix_web::rt::spawn` (actix's
  per-worker, single-threaded runtime, the same as `socket.rs:224` does for the
  Jellyfin socket). Handlers reach shared state through the existing
  `AppState` (actors and mpsc per V18), never by moving RPC objects across
  threads. In wasm, the browser's single thread and `wasm-bindgen-futures`
  `spawn_local` drive it.
- **Consequence**: per-connection work is pinned to one worker. That is fine for
  a household server; recorded in case connection counts grow.

## R3 — transport adapters

- **Decision**: a `Transport` trait in `pharos-client` yielding
  `(impl AsyncRead, impl AsyncWrite)`. Browser: a `web_sys::WebSocket` in
  `arraybuffer` mode wrapped as a byte stream (each RPC frame may span or share
  WebSocket messages; capnp's own framing handles boundaries). Native: tokio
  TCP/TLS through `tokio-util` compat. Server: `actix-ws` (already a dependency,
  0.4) wrapped the same way.
- **Alternative**: one capnp message per WebSocket message. Rejected: it couples
  the RPC framing to WebSocket framing for no gain.

## R4 — instrumentation in the generated dispatch layer (V173)

- **Finding**: `capnpc` generates a `Server` trait per interface plus a
  `dispatch_call` that matches on method ordinal. There is no middleware hook.
- **Decision**: a small build-time generator in `pharos-api/build.rs` runs after
  `capnpc` and emits, for every interface, an `Instrumented<S>` wrapper that
  implements the generated `Server` trait by delegating to `S`. Each method
  opens the `rpc.<iface>.<method>` span from the call context, times it, maps
  the result to an outcome label, logs failures with the cause, and counts
  cancellation via a drop guard. Servers are registered only as
  `Instrumented<S>`; the raw `S` is not constructible as a capability outside
  the crate (a private constructor), so an uninstrumented method cannot ship.
- **Evidence needed**: T010 proves the wrapper with a failing handler and a
  disarm check, as the spec's §Observability tests require.

## R5 — shared services: extracting from Jellyfin handlers

- **Finding**: the logic the first native methods need lives inside actix
  handlers that return `HttpResponse`, e.g. `items.rs:3589 latest_items` and
  `items.rs:7194 resume_items` (which also parses `MediaTypes` from the raw
  query string).
- **Decision**: per behaviour, extract a function in a new
  `pharos-server/src/services/` module taking typed inputs (`UserId`,
  `ParentFilter`, `Vec<MediaKind>`, paging) and returning domain types. The
  Jellyfin handler becomes parse → service → DTO, and the native method becomes
  schema → service → capnp builder. The Jellyfin integration tests that already
  cover each handler are the safety net and must stay green unchanged.
- **First tranche** (enough for home + library + detail + playback + events):
  views, latest, resume, next up, items query, item detail, user data
  toggles, playback negotiate/report, event fan-out.

## R6 — session cookie on the WebSocket upgrade (Clarifications Q3)

- **Decision**: browsers cannot set cookies from a WebSocket frame, so browser
  login is a plain `POST /pharos/session` (JSON in, `Set-Cookie` out) followed
  by the upgrade. Native apps use `Bootstrap.login` / Quick Connect over RPC
  and keep the returned device token. `resume()` on the bootstrap then reads the cookie the upgrade
  carried. Cookie: `pharos_session`, `HttpOnly; Secure; Path=/pharos;
  SameSite=Strict` (or `None` for cross-server, 017 FR-003a). The `Origin`
  allow-list defaults to the server's own origin plus `[server].allowed_origins`.
- Token material lives in the existing hashed token store (ADR-0011); the cookie
  carries the token, never a new secret type.

## R7 — schema compatibility check (FR-009)

- **Decision**: `capnp compile -o-` emits a `CodeGeneratorRequest`; a test in
  `pharos-api` loads the `main` branch's schema (checked in as
  `schema/compat/main.bin` and refreshed by a `just` recipe on release) and
  fails if any struct field or method ordinal changed type or name, or was removed.

## R8 — baseline numbers for gates

| Measure | Value | Notes |
|---|---|---|
| Current pharos-ui wasm, workspace release profile | 3.53 MB raw / 724 KB gzip | not stripped, no wasm-opt |
| Same, `opt-level=z`, LTO, 1 CGU, stripped | 1.86 MB raw / 525 KB gzip | still no wasm-opt / wasm-bindgen post-processing |
| capnp + capnp-rpc spike (standalone) | 343 KB raw / 112 KB gzip | upper bound on increment |

The table above is the original host-cargo run, kept for reference. The devShell
figures (the same three rows) are in quickstart.md §0.

The SC-002 payload and decode comparison needs a server, so it is task T040,
against the seeded fixture library.
