# ADR-0029: Native API transport: per-worker RPC over a WebSocket, sessions resumed from a cookie or device token

- **Status:** Accepted
- **Date:** 2026-11-01T13:40:00Z
- **Deciders:** Alison

## Context

ADR-0028 chose Cap'n Proto RPC for pharos-ui and future pharos apps. Four
things still had to be settled for it to carry traffic:

- capnp-rpc's `RpcSystem` and capabilities use `Rc` internally, so they are
  `!Send`, while actix serves requests on a pool of worker threads.
- Browsers can only reach the server over HTTP and WebSocket, and native apps
  should not need a second protocol.
- Browsers cannot set cookies from a WebSocket frame, and a WebSocket upgrade
  is outside CORS, so the session cookie that rides on it is exposed to
  cross-site WebSocket hijacking.
- `resume → session.library() → library.home()` must be one round trip
  (spec 017 SC-001), and capnp-rust generates no pipeline accessor for a
  capability inside a union (research R1).

## Decision

**Transport (R2, R3).** We will serve the API at `GET /pharos/rpc` as a
WebSocket carrying capnp's own byte stream. Inbound binary messages are
written into a bounded in-memory pipe, the `RpcSystem` reads and writes the
other end, and its output goes back out as binary messages. Message
boundaries carry no meaning, and the pipe bound is the backpressure. Each
connection's `RpcSystem` runs on the actix worker that accepted the upgrade
(`actix_web::rt::spawn`). Handlers reach shared state only through
`AppState`, never by moving RPC objects across threads. The dispatch
(`rpc::bootstrap_client`) knows nothing of the transport, so a native client
on plain TCP and the tests use the same code.

**Admission.** The server refuses an upgrade before it happens in two cases:
- An `Origin` header present that is neither this server's own host nor an
  exact `[server.native_api].allowed_origins` entry gets 403. No `Origin`
  means a native app and is allowed; there is no wildcard.
- When 64 connections are already open without a session, the upgrade gets
  429.

A connection is closed when:
- it has not resumed a session within 30 s;
- it has been idle for 120 s;
- it sends a text frame;
- its `RpcSystem` stops.

Each signed-in user may hold at most `max_connections_per_user`
connections. Incoming messages are decoded with an 8 MiB traversal limit
and nesting 32.

**Sessions (R1, R6).** A browser signs in with `POST /pharos/session` (JSON
in, `Set-Cookie: pharos_session=<token>; HttpOnly; Secure; Path=/pharos;
SameSite=Strict` out); the token is an ordinary ADR-0011 device token. A
native app keeps the device token from `Bootstrap.login` or Quick Connect.
`Bootstrap.resume(deviceToken)` resolves the token, or the upgrade's cookie
when it is empty, on every call, so a revocation takes effect at the next
resume.

`resume` always answers with a `Session` capability plus a `status`. A
refused session is the generated `Revoked` handler, which fails every call
with `Error.kind.revoked`, and its capability getters return `Revoked`
capabilities. So a pipelined call still costs one flight, and it is counted
`revoked`, not `unimplemented`. A token-store outage answers `unavailable`,
not `revoked` (B216).

**Off by default.** Every push to main deploys (ADR-0010), so the routes
exist only when `[server.native_api].enabled = true`. Startup logs the state
and publishes `pharos_native_api_enabled`.

## Consequences

- A connection's work is pinned to one worker. That is fine for a household
  server. If connection counts grow, revisit with a dedicated `LocalSet`
  pool.
- Browser and native clients share one endpoint and one dispatch; tests
  drive the dispatch in-thread, so most observability assertions need no
  socket.
- The Origin rule and the decode/connection limits are the security
  boundary of the browser path. Changing any of them needs a security
  review.
- A refused session still costs a capability object per call chain. That is
  the price of keeping refusals pipelined.
- Rate limiting of password checks is shared with the Jellyfin path (V174)
  and is not part of this transport.

## Alternatives considered

- **One capnp message per WebSocket message.** This couples RPC framing to
  WebSocket framing for no gain, and capnp already frames its messages.
- **A `Send` RPC runtime, or a thread per connection.** capnp-rpc is
  `!Send` by design. Actix workers are already single-threaded runtimes,
  which is what it wants.
- **`resume` returning a union of session or error.** This is the schema
  convention, but capnp-rust cannot pipeline through it, so the first call
  after resume would cost a second flight (R1, decided by Alison).
- **Relying on the cookie's `SameSite=Strict` instead of an Origin check.**
  SameSite does not cover every browser and same-site subdomain case, and
  the check costs nothing.

## References

- specs/017-native-api: spec.md FR-003/FR-003a/FR-010, research.md R1–R3,
  R6, data-model.md
- `crates/pharos-server/src/api/pharos/rpc/` (mod.rs, socket.rs, origin.rs,
  limits.rs, bootstrap.rs)
- ADR-0010 (deploy on push), ADR-0011 (token store), ADR-0028 (native API)
