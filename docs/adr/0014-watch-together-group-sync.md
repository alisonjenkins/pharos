# ADR-0014: Watch-together group-sync over a WebSocket, Jellyfin SyncPlay-compatible

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

"Watch together" means a leader's play/pause/seek intent is applied by every
follower at the same instant, so their audio waveforms line up within
human-imperceptible drift (target: 500 ms p95, invariant V3). Jellyfin already
has SyncPlay, and existing phone/TV clients are first-class targets (ADR-0001,
V20) — so we cannot invent a fresh protocol and drop those clients. Jellyfin's
own implementation also has failure modes worth improving on (V19): a single
buffering member or a leader disconnect degrades the whole group.

## Decision

Group sync runs over a **WebSocket**, with an actor holding all mutable group
state (V18). There are two wire framings over **one shared server algorithm**:

- **Jellyfin path (canonical for unmodified clients)** — the standard Jellyfin
  `/socket` WebSocket, multiplexed by the `MessageType` field. pharos handles
  the SyncPlay subset (`SyncPlayJoinGroup`, `SyncPlayPlay`, `SyncPlayPause`,
  `SyncPlaySeek`, `SyncPlayBuffering`, `SyncPlayPing`, …) inbound and emits
  `SyncPlayGroupUpdate` / `SyncPlayCommand` / `SyncPlayPong`. A translation
  layer maps each Jellyfin message to/from the internal `ClientMsg`/`ServerMsg`
  enums — the actor never sees Jellyfin shapes. `GET /socket` lives in
  `crates/pharos-server/src/api/jellyfin/socket.rs`; the group logic is in the
  `pharos-sync` crate (`group`, `registry`, `messages`).
- **Extended path (pharos-native / Dioxus clients)** — a richer framing at
  `/sync/v1/ws` for features Jellyfin's fixed shape can't carry.

Sync is server-clock scheduled: the actor stamps each command with an
`at_server_ms` computed from a single monotonic reference plus a lead time
(300 ms), and each follower estimates its own clock **offset** by an NTP-style
4-timestamp round trip (9 samples on join, median published to kill jitter),
then schedules the action via `sleep_until(at_server_ms − offset)`. Members
route to a group actor through a `GroupRegistry` (`HashMap<GroupId, mpsc::Sender>`);
the actor fans `ServerMsg` out to per-member sinks. Auth tokens ride in the
handshake as `SecretString`, consumed before the actor inbox so they never reach
logs or trace fields (V8).

## Consequences

- Unmodified Jellyfin phone/TV clients join pharos groups with zero client
  changes, and inherit pharos's better algorithm — per-member offset, isolated
  buffering, deterministic leader handoff — without knowing it exists (V19).
- pharos-native clients get the extended `/sync/v1/ws` framing for richer
  member-state and future wire encodings that Jellyfin's JSON-only shape forbids.
- Two framings are a maintenance cost, bounded by the fact that both collapse to
  the same `ClientMsg`/`ServerMsg` actor — the sync algorithm is written once.
- Failure handling is explicit: 3 missed heartbeats evict a member; a member
  buffering > 1 s triggers a group-wide corrective `Pause`; leader loss elects
  the lowest `MemberId` deterministically (no voting).

## References

- `crates/pharos-server/src/api/jellyfin/socket.rs`; `crates/pharos-sync/`
- `docs/group-sync-protocol.md`; `SPEC.md` §V (V3, V8, V18, V19, V20)
- ADR-0001 (Jellyfin API as the client contract)
