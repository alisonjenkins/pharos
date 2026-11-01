# Feature Specification: pharos native API (Cap'n Proto) for pharos-ui

**Feature directory**: `specs/017-native-api/`
**Created**: 2026-11-01
**Status**: clarified, planned and tasked (`/speckit-tasks` 2026-11-01); implementation starts at tasks.md Phase 1
**Input**: "We will still maintain compatibility with the Jellyfin clients,
however Pharos was always meant to be the next gen. We will be having our own
API endpoints but reusing the code we have written for the Jellyfin clients
where appropriate. In particular we are going to look to make the
communication between the UI and the backend more efficient by using things
like Cap'n Proto." (Alison, 2026-11-01)
**Decision record**: ADR-0028 (native API for pharos-ui; Jellyfin API remains
for third-party clients). V16 amended, V172 added.
**Depends on**: none. **Enables**: 009 (`PharosApi`, `LiveChannel` ports), 010,
012–016.

## Goal

pharos-ui talks to pharos over a **pharos-native, schema-first, binary API**
built on Cap'n Proto, designed for pharos's model and for the UI's access
patterns, instead of over the Jellyfin REST API. The Jellyfin API stays as a
supported compatibility surface for third-party clients (principle I, V1, V7),
and both surfaces sit on **shared domain services**, so logic is written once.

## Clarifications

### Session 2026-11-01

- Q: Full Cap'n Proto RPC, or plain capnp messages with our own framing? → A: Full capnp-rpc (pipelining, capabilities, callbacks).
- Q: Can new UI features ship on the old REST client before their native methods exist? → A: No. Native-first: no new UI feature uses the REST adapter; existing panes migrate when touched.
- Q: After a page reload, how does the web UI get its session back? → A: HttpOnly cookie for browsers (set at login, presented on the WebSocket upgrade); tokens for native apps.
- Q: Should payload and decode gains be a hard target? → A: Measure first in the plan's spike, then set the gate from the measured numbers; CI guards against regression.
- Q: Should the client half of the API be a shared Rust core for future apps? → A: Yes. A platform-neutral Rust client core, used by the web UI now and bindable by phone/TV apps (UniFFI) later.

## Clients this API is for

The web UI (009–016) is the **initial focus** and the first consumer. pharos
will later ship its **own phone and TV apps** (Alison, 2026-11-01), and the
native API is their contract too. So:

- Schemas are designed around pharos's model, not the web UI's screens.
  Pipelining follows **capabilities, not data**: a call whose input is data
  returned by another call (e.g. "latest items for each view id") must wait
  for it. Where a screen needs such a data-dependent fan-out on every load,
  the server provides one aggregate method that every client can use
  (`Library.home`), rather than each client paying an extra round trip.
- Transport is pluggable: binary WebSocket for browsers, and **plain TCP/TLS
  capnp-rpc** for native apps (no browser restriction there). Same schema,
  same server dispatch.
- Nothing in the schema assumes a DOM, a pointer or a screen size. TV-style
  spatial navigation and phone touch are client concerns.
- Clients share a **Rust client core** (FR-001a): the web UI uses it now, and
  phone and TV apps bind it via UniFFI later instead of reimplementing
  Cap'n Proto RPC in Kotlin/Swift, where library support is thin.

## Why Cap'n Proto fits pharos

- **Fewer round trips.** Promise pipelining chains capability calls
  (`resume → session → library → home`) into one flight without waiting on
  each result, and an aggregate method covers the data-dependent part. Home
  renders in one round trip instead of the 6–10 jellyfin-web makes.
- **Server push on the same connection.** Callback capabilities replace the
  Jellyfin `/socket` for pharos-ui: subscribe to user data, library changes,
  sessions, group sync, task progress.
- **Capabilities as security.** A login returns a session capability, and
  holding a playback capability lets the holder fetch one item's media. This is
  the model B75 already moved toward with the `tag` capability token.
- **Schemas are append-only by ordinal**, which matches the project rule
  "never renumber; append". Old UIs keep working against newer servers.
- **One schema, Rust on both ends.** `capnp` generates Rust for the server and
  for the wasm UI, extending ADR-0012's shared-types benefit to a wire contract.
- **Cheap to decode.** Zero-copy reads in wasm, where JSON parsing of large item
  lists is a measurable cost.

## Scope

**In**: every UI↔server interaction except bulk media bytes. That covers auth,
browse, search, item detail, user data, preferences, themes, playback
negotiation and reporting, group sync, sessions and remote control, admin, and
telemetry (016).

**Out (stay plain HTTP)**: HLS playlists and segments, direct streams, images,
trickplay sprites, subtitle files, book files. These need browser caching,
range requests, `<img>`/MSE consumption and the existing segment cache. The
native API hands out their URLs (with capability tokens) instead of their bytes.

## Observability: parity with HTTP, enforced structurally

**Requested by Alison, 2026-11-01**: "We need to make sure it is all still
observable." A binary RPC wire must never be harder to diagnose than the
HTTP surface it sits beside. Today every HTTP request gets a `http.request`
span (method, URI, request_id), RED metrics by route pattern, an abort counter
and JSON logs carrying `trace_id`. The native API MUST match that, method for
method, and must not depend on each handler remembering to do it.

### Rule

Instrumentation lives in the **generated dispatch layer**, not in handlers.
`crates/pharos-api`'s server stubs wrap every method in one instrumented
wrapper, so a method that exists is a method that is observed. A handler cannot
opt out. The client side (pharos-ui's native `PharosApi` adapter) wraps every
call the same way, so 016 sees both ends.

### What every call records (server)

| Signal | HTTP today | Native API |
|---|---|---|
| Span per request | `http.request` | `rpc.<interface>.<method>`, parented on the call context (FR-006); pipelined calls appear as children of the call they depend on, so a pipelined batch reads as one tree in Tempo |
| Request id | `request_id` | `call_id` (connection id + question id), in span and logs |
| RED counter | `http_requests_total{method,path,status}` | `pharos_rpc_calls_total{interface,method,outcome}` (outcome is a closed set from `Outcome::label()`: `ok`; one per `Error.kind` variant: `not_found`, `not_permitted`, `invalid`, `limit`, `revoked`, `unavailable`, `other`; `failed` for a transport/handler `capnp::Error`; `disconnected`; `unimplemented` (no handler for the method, e.g. a client ahead of the server); `cancelled`) |
| Latency | `http_request_duration_seconds{method,path}` | `pharos_rpc_duration_seconds{interface,method}` (histogram; long-running methods added to `LONG_RUNNING_SECONDS`) |
| Client abort | `http_client_aborted_total{method,path}` | `pharos_rpc_cancelled_total{interface,method}` (caller dropped or disconnected mid-call) |
| Failure log | JSON line with status + cause | WARN/ERROR line per failed call: interface, method, error variant **and its cause value**, call_id, trace_id, user, device. Symmetric with success: the success path never records a field the failure path omits |
| Payload size | — | `pharos_rpc_bytes_total{direction}` plus per-call request/response size on the span |
| Debug detail | request logging (`logAllRequests`) | decoded, redacted params and results at debug level, toggled per interface at runtime by `PHAROS_LOG_LEVEL` directives (`pharos_api::rpc::library=debug`) |

### Connection and push lifecycle

- `pharos_rpc_connections{transport}` gauge; connect / close logged with
  transport, client version, device, duration and **close reason** (client
  close code, keep-alive timeout, server shutdown, limit, protocol error with
  the decode error text).
- Capability lifecycle: issued / released / revoked counts by type, so a leak
  shows as a growing gauge, not a mystery OOM.
- Subscriptions: `pharos_rpc_subscriptions{topic}` gauge,
  `pharos_rpc_events_total{topic,outcome}` (delivered / dropped / coalesced),
  and `pharos_rpc_event_lag_seconds{topic}` (event produced → callback
  acknowledged). A slow subscriber is visible by name, not inferred.
- Limits (FR-010): every refusal increments `pharos_rpc_limit_total{limit}` and
  logs the limit and the value that crossed it.
- Schema/version: `pharos_rpc_client_schema_total{version}` and an
  `Unimplemented` counter by method, so a UI that is ahead of or behind the
  server shows up by query.

### Client side (feeds 016)

The native adapter records, per call: interface, method, latency, outcome,
bytes, call_id and trace id, into the 016 telemetry facility (sent only when
the user opted in) and always into the local debug overlay, which shows
decoded calls, events and capability counts live.

### Tooling so binary is never opaque

- `pharos rpc call <interface>.<method> --json '{…}'` CLI: calls any method
  with JSON-encoded params (schema-driven), prints decoded results.
- `pharos rpc tap` (admin, dev): streams decoded, redacted call/event summaries
  for one user or device, for live debugging without packet capture.
- Loki gets decoded summaries, never raw bytes.

### Named queries (added to `docs/observability.md`)

- Failing methods now:
  `sum by (interface,method,outcome) (rate(pharos_rpc_calls_total{outcome!="ok"}[5m]))`
- Slow methods:
  `histogram_quantile(0.95, sum by (le,interface,method) (rate(pharos_rpc_duration_seconds_bucket[5m])))`
- One user's failed calls:
  `{namespace="pharos"} | json | target=~"pharos_api::rpc.*" | level=~"WARN|ERROR" | user="<id>"`
- Push falling behind:
  `max by (topic) (pharos_rpc_event_lag_seconds)`, `rate(pharos_rpc_events_total{outcome="dropped"}[5m])`

### Tests (constitution III)

- A test calls a generated method with a handler that fails, and asserts the
  span, the counter increment with the right outcome label, and the failure log
  line carrying the cause value. Disarming the dispatch wrapper makes it fail.
- A label-distinctness test over every generated `label()` (interface, method,
  outcome, topic, limit).
- A redaction test: every method whose schema has a field annotated
  `$secret` is called with a known secret; neither logs, spans nor the tap
  contain it.
- A parity test enumerates the schema and asserts every method has RED series
  pre-registered at zero, so `rate()` works from the first scrape (as
  `pharos_segment_produced_total` does).

## User Stories

### US1 — home in one round trip (P1)

Opening home issues one pipelined flight (`resume → session.library →
library.home`) and renders every rail from its single result.

**Independent test**: count network round trips for home in pharos-ui (native)
vs jellyfin-web (REST) against the same seeded library; native ≤ 2 (connect
+ batch).

### US2 — live updates without polling (P1)

The UI subscribes once and receives typed events for user data, library,
sessions, group and tasks over the same connection (replaces 009 FR-004's
use of `/socket` for pharos-ui).

### US3 — Jellyfin clients unaffected (P1)

No Jellyfin endpoint changes shape or behaviour because of this work. The
full existing compat suite (`client_compat.rs`, Playwright jellyfin-web,
SyncPlay e2e) stays green at every step.

### US4 — one implementation of each behaviour (P1)

A behaviour both surfaces expose (resume items, next up, playback negotiation,
mark played, group commands) is implemented once in a domain service. The
Jellyfin handler and the native method are thin translations over it.

**Independent test**: for each migrated behaviour, the same service-level test
covers both surfaces, and the two wire adapters contain no branching business
logic (reviewed against a checklist in `/speckit-plan`).

### US5 — debuggable binary (P1)

A binary protocol must not hide problems (016). Every call is logged and
traced by interface + method with decoded, redacted parameters at debug level;
the 016 debug overlay shows decoded calls and events; a `pharos rpc` CLI
subcommand can call any method and pretty-print results.

### US6 — evolve safely (P2)

Adding a field or method never breaks a deployed UI or server one version
apart. A CI check rejects schema changes that renumber, retype or remove
ordinals.

## Functional Requirements

- **FR-001** Schema crate `crates/pharos-api` holding `.capnp` schemas and
  generated Rust for both targets (native + `wasm32-unknown-unknown`). `capnp`
  compiler pinned in the Nix devShell. Excluded from `workspace-hack` for the
  wasm build like `pharos-ui` is.
- **FR-001a** Client core crate `crates/pharos-client`: connection and
  reconnect, session resume, the RPC client over `crates/pharos-api`, call
  instrumentation (V173 client side), query cache primitives and event
  dispatch. Platform-neutral: no `web-sys`, no DOM, no UI types. Transport is a
  trait with a browser WebSocket implementation (wasm) and a TCP/TLS
  implementation (native, used by tests now and apps later). pharos-ui's native
  `PharosApi` and `LiveChannel` adapters (009) are thin wrappers over it. Its
  public surface is kept UniFFI-compatible (owned types, no lifetimes or
  generics across the boundary) so app bindings need no redesign.
- **FR-002** Transport: Cap'n Proto RPC over a **binary WebSocket** at
  `/pharos/rpc` for browsers (no raw TCP), with a TCP/TLS listener for future
  native apps sharing the same dispatch. v1 ships the WebSocket only. One connection per tab; reconnect
  with backoff restores subscriptions. Full **capnp-rpc** (level 1: promise pipelining, capability passing,
  callback capabilities for push); not plain messages with custom framing.
- **FR-003** The bootstrap interface exposes only `serverInfo`, `resume`,
  `login`, `quickConnectInitiate`/`quickConnectPoll` and (when advertised)
  `submitAnonymous`. Native apps call `login` or Quick Connect and receive a
  `Session` plus a device token to store. Browsers log in over
  `POST /pharos/session`, which sets the cookie (FR-003a, research R6), and then
  call `resume` with an empty token. Every route ends in a `Session` capability. Every other
  interface is reached from it, scoped by the user's policy (admin interfaces
  only for admins). Tokens from the Jellyfin auth path and native sessions share
  one store (ADR-0011) so a user's devices list stays one list.
- **FR-003a** Session persistence. **Browsers**: a successful login sets an
  `HttpOnly`, `Secure` session cookie scoped to `Path=/pharos` (so both `/pharos/session` and
  `/pharos/rpc` receive it); the WebSocket
  upgrade presents it and the bootstrap's `resume()` returns the `Session`
  capability without the token ever being readable by script. Same-origin
  deployments use `SameSite=Strict`; connecting the UI to another pharos server
  (T59 multi-server) uses `SameSite=None` and that server's own cookie. The
  upgrade checks `Origin` against an allow-list to stop cross-site WebSocket
  hijacking. Logout and revoke (015 devices) clear the cookie and drop live
  connections. **Native apps**: present a stored device token to `resume()`.
  Both resolve to the same token store (ADR-0011). No session token is kept in
  `localStorage`; the current `client.rs` habit is removed as panes migrate.
- **FR-004** Interfaces (initial cut, refined in `/speckit-plan`): `Library`
  (views, items query, item detail, people, genres, search), `UserData`,
  `Prefs` (incl. themes 010, keymaps 011), `Playback` (negotiate → returns
  stream URLs + a playback capability; report progress), `Queue`, `Group`
  (014, carrying the pharos group-sync protocol natively, V20), `Sessions`
  (remote control), `RemoteSources` (008), `Admin` (015), `Telemetry` (016),
  `Events` (subscription with a callback capability).
- **FR-005** Shared domain services: before a native method is written, the
  logic it needs is moved out of `api/jellyfin/*` into a service module both
  call, with the existing Jellyfin tests as the regression net. No copy-paste
  between wire layers. Extraction happens per behaviour, as native methods land,
  not as a big-bang refactor.
- **FR-006** Call context on every call: W3C trace context, UI route/action
  (016 FR-001/FR-002), device id, client version. The server opens a span per
  call named `rpc.<interface>.<method>`, parented on the incoming context.
- **FR-007** Errors are typed per method (an error union), carry the cause
  value (constitution IV) and a trace id, and map to the same domain error
  types the Jellyfin layer maps from.
- **FR-008** Observability per §Observability: generated, non-optional
  instrumentation for every method, connection, capability and subscription,
  at parity with the HTTP surface, with the named queries and tests listed
  there. Guarded by V173.
- **FR-009** Schema-compat CI check (US6) comparing against the schema on `main`.
- **FR-010** Limits: per-connection message size, call concurrency, subscription
  count; exceeding one returns a typed error and increments a counter naming the
  limit.
- **FR-011** Debug tooling per US5: decoded-call logging at debug level with
  V8 redaction (generated `Debug` impls skip fields annotated as secret), a
  `pharos rpc` CLI, and a decoder for the 016 overlay.
- **FR-012** Migration: pharos-ui's `PharosApi` port (009 §Boundaries) gets a
  native adapter. **Native-first**: no new UI feature (any 010–016 story) may
  ship on the Jellyfin REST adapter; its native method and shared service land
  first, in the same feature. The current `client.rs` (Jellyfin REST) remains
  only for the panes that exist today (T55/T58/T59 and the current
  browse/detail/player views), each migrating to native the next time it is
  touched, and is deleted when the last one has moved. A test fails if a view
  added after this spec reaches the REST adapter. The UI's views never know
  which adapter served them.

## Success criteria

- **SC-001** Home, library page and item detail each render from ≤ 1 batch
  after connect (US1), measured in Playwright.
- **SC-002** Payload bytes and client decode time for a 500-item library page,
  native vs Jellyfin JSON. Task T040 measures both on the
  seeded library (the plan's spike could not, as it needs a running server) and sets the pass gate from the result (recorded in
  `quickstart.md`); afterwards a CI benchmark fails on a regression of more than
  10 % against that gate. The native API must at least not be worse than JSON on
  either measure.
- **SC-003** The Jellyfin compat suite is green at every commit of this feature.
- **SC-004** By query after deploy: `pharos_rpc_calls_total` by method and
  outcome, p95 `pharos_rpc_duration_seconds` by method, recorded.
- **SC-005** Every row of the §Observability parity table is asserted by a test that fails with the dispatch wrapper disarmed.

## Edge cases

- Cookie blocked (third-party-cookie restrictions on a cross-server connection): the UI says so and offers to open that server's own UI, instead of silently failing to resume; logged with the browser's reason where known.

- Proxy or ingress that drops idle WebSockets: keep-alive pings below the
  ingress timeout. Angie sidecar config updated for `/pharos/rpc` upgrade.
- Server rolling deploy: connections drop, UI reconnects to the new pod and
  re-subscribes; in-flight calls fail with a typed `Disconnected`, are retried
  if idempotent, and are surfaced to the user if not.
- Multi-replica (postgres): subscriptions on replica A must see events produced
  on B. Same fan-out problem as ADR-0016; reuse its distribution.
- A UI one schema version ahead of the server: unknown methods return
  `Unimplemented`; the UI hides the feature and records why.

## Alternatives to record in the ADR at `/speckit-plan`

Protobuf + gRPC-web (needs a proxy, no pipelining, no capabilities), FlatBuffers
(zero-copy but no RPC system), JSON over a WebSocket (simple, but the
inefficiency we are trying to remove), and keeping Jellyfin REST (no push or
pipelining, and Jellyfin-shaped data).
