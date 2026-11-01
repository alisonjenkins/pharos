# Data model: 017-native-api

The wire model is the Cap'n Proto schema in [contracts/pharos.capnp](./contracts/pharos.capnp).
This file lists the entities and the rules the schema alone cannot express.

## Ids

- `ItemId`, `UserId`, `GroupId`, `PlaySessionId`, `DeviceId` are distinct
  structs, never interchangeable bare `Text` (constitution V on the wire).
  `ItemId` is a union `real :UInt64 | synth :UInt64` mirroring
  `pharos_jellyfin_api::WireId`; `UserId` is the 16-byte UUID of
  `pharos_core::auth::UserId`.
- The Jellyfin layer's string ids map 1:1 (same underlying values), so an item
  is the same item on both surfaces.

## Time

- Durations and positions are `UInt64` **milliseconds** (`Millis`), not
  Jellyfin ticks. Conversion happens only in the Jellyfin wire layer (T88b).

## Session lifecycle

`Unauthenticated` → (`POST /pharos/session` or Quick Connect) → cookie or
device token → (upgrade + `resume()`) → `Session` capability → (logout,
revoke, expiry) → all capabilities derived from it are revoked and calls
return `Revoked`.

## Capabilities

| Capability | Obtained from | Scope | Revoked when |
|---|---|---|---|
| `Session` | `Bootstrap.resume` | one user, one device | logout, revoke, token expiry |
| `Library`, `UserData`, `Prefs`, `Playback`, `Queue`, `Group`, `Sessions`, `RemoteSources`, `Telemetry`, `Events` | `Session` getters | the session's user, filtered by policy | session revoked |
| `Admin` | `Session.admin()` | admins only; `NotPermitted` otherwise | session revoked or admin flag removed |
| `PlaybackGrant` | `Playback.negotiate` | one item, one `PlaySessionId`; carries the media URLs | stop, expiry, session revoked |
| `EventSink` (client-provided) | passed to `Events.subscribe` | server calls it | client releases, disconnect |

## Errors

Every method's result is a union of `ok` and a method-specific error struct.
All errors carry `reason :Text` (the cause value) and `traceId :Text`. Common
variants are `NotFound{id}`, `NotPermitted{what}`, `Invalid{field, value}`,
`Limit{name, value, max}`, `Revoked`, `Unavailable{dependency}`.

## Events (push)

`Event` is a union of `userData{itemId, played, favorite, positionMs}`,
`libraryChanged{added, updated, removed}`, `session{...}`, `group{...}`,
`task{id, progress, state}`, `activity{...}`, `resync`. `seq :UInt64` is per
**user event stream**, monotonic across connections, devices and replicas (the
event's position in the user's stream, assigned where the event is published).
The server keeps a bounded replay window per user (default 10 000 events or
15 min). `subscribe(afterSeq)` replays everything after `afterSeq` that is
still in the window. If `afterSeq` is older than the window, the server sends
one `resync` and the client drops its cached queries. Gaps and resyncs are
counted (`pharos_rpc_events_total{outcome="resync"}`).

## Conventions

- Every method takes `ctx :CallContext` first, which carries `device`
  (FR-006, V173). Capability getters do too, so their spans join the trace.
- Every method returns a result union of ok and `Error`. Methods with no
  payload return `Done` (FR-007). The one exception is the `Session`
  capability getters, which cannot fail: a revoked session surfaces as
  `Error.kind.revoked` on the first call made through the returned capability.
- An optional scalar is an explicit union (`OptU32`, `OptU16`, `StreamChoice`),
  never a zero default. Season 0, episode 0 and subtitle "none" are real values.
- An optional struct is a pointer field; absent means "not applicable".
- Closed vocabularies are enums (`Kind`, `EventKind`, `Level`), so the
  append-only rule (FR-009) can add members but never needs to retype a `Text`.
