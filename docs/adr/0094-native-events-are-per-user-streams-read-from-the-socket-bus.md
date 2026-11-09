# ADR-0094: Native events are per-user streams read from the socket bus

- **Status:** Proposed
- **Date:** 2026-11-09T23:30:00Z
- **Deciders:** Alison (pending review), Claude

## Context

Spec 017 US2 asks for live push to first-party clients over the native API:
`Events.subscribe(sink, topics, afterSeq)` delivers a user's changes to an
`EventSink` capability, with `seq` per user stream, a bounded replay window
(10 000 events or 15 min, data-model §Events) and one `resync` when a client
resumes from outside it. The schema has carried the interface since schema
version 1, but the server had no implementation.

Every change the push needs is already announced: the services publish
`SocketBroadcast` on `AppState::bus` for jellyfin-web's `/socket`
(user data with full `UserItemDataDto` entries, library deltas, scan
progress). RAM on the deployment is scarce, and one replica runs today.

## Decision

We will build the event stream as one in-memory `services::events::EventLog`
on `AppState`, fed by a single reader of the existing bus:

- The reader starts on the first `subscribe`, so a server nobody subscribes
  to keeps nothing and runs nothing extra.
- A user's stream exists only once that user has subscribed. Events for a
  user with no stream are not kept. A library change or task update reaches
  every stream, and its payload is stored once (`Arc`) however many streams
  hold it.
- A new stream's first `seq` is the wall clock in microseconds. A `seq` from
  before a restart is below the floor and resumes as one `resync`, never as
  a replay of unrelated events. A `seq` above the head (another run or
  replica) resyncs too.
- `afterSeq = 0` starts live with no replay. A subscriber that falls behind
  the live channel catches up from the window by `seq`. If the bus reader
  itself lags, every stream gets a `resync`.
- Bus entries are translated in one place. A user data entry is read back
  from its `UserItemDataDto` JSON (`ItemId`, `Played`, `IsFavorite`,
  `PlaybackPositionTicks`, `PlayCount`). An entry it cannot read is
  logged with its value and counted `undecodable`.
- Topics on the wire are `userData`, `library` and `task`; an empty list means
  all of them, and `resync` always arrives. A connection holds at most 8
  subscriptions (`limit subscriptions`). A pump stops once its session logged
  out on the connection.

## Consequences

- There are no new publish sites. Any service that announces on the bus is
  pushed to native clients as well, and `/socket` is unchanged (US3).
- Memory is bounded per subscribed user, by the window: about 40 bytes per
  event plus shared payloads.
- The window lives in one process. Multi-replica distribution (T044, ADR-0016's
  bus) is still open: a subscriber on replica A does not see events from
  B, and a `seq` from another replica resyncs. This is acceptable while one
  replica runs.
- User data events are decoded from JSON built for jellyfin-web, so a change
  to `UserItemDataDto`'s field names would silently stop native user data
  events. Two things guard against that: the `undecodable` counter, and the
  unit test `a_user_data_announcement_becomes_typed_events`, which builds a
  real DTO.
- Sessions, group and activity events (data-model §Events) are not in the
  schema's `Event` union yet. They need a schema bump of their own.

## Alternatives considered

- **Typed events at every publish site** (a second call next to each
  `notify_*`): no JSON round trip, but every site would need two calls, and a
  missed one would split the two surfaces. This was rejected for now. If
  the JSON decoding proves fragile, the bus variant can carry the typed
  state instead.
- **Durable stream in the database**: survives restarts and works across
  replicas, but it writes on every progress tick, and `resync` already
  covers a restart. This was rejected while one replica runs.
- **Keep streams for every user from boot**: lets a client resume across a
  window it never subscribed in, but holds memory for users who never
  use the native UI. This was rejected because of the RAM budget.

## References

- specs/017-native-api/data-model.md §Events, spec.md US2, tasks T026a, T041–T045
- crates/pharos-server/src/services/events.rs, crates/pharos-server/src/api/pharos/rpc/events.rs
- ADR-0016 (multi-replica distribution), ADR-0028 (native API)
