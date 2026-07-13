# ADR-0016: Durable, multi-replica SyncPlay (persisted groups + advisory-lock ownership)

- **Status:** Accepted
- **Date:** 2026-07-13T00:00:00Z (decisions live 2026-07-11 … 2026-07-12)
- **Deciders:** Alison

## Context

ADR-0014's group actor holds all state in memory: a process restart (deploy,
crash) destroyed every group, and jellyfin-web gives the user no way to recover
short of leaving and recreating the group. With rolling deploys (ADR-0015) a
restart happens on every push to `main` — and during the surge window **two
replicas run concurrently**, so a group's members can be connected to
*different processes*. In-memory actors also made stuck groups permanent: a
member whose tab died stayed in the readiness gate forever.

Live incidents drove this: a pause that only paused the presser, a member
wedged on a frozen frame unable to leave, groups accumulating as stale rows
(SPEC §B B24–B33).

## Decision

Three layers on top of the ADR-0014 actor (which is unchanged in its core):

1. **Durability.** Every group actor snapshots its state to a `sync_groups`
   table via a `GroupPersistence` trait on every mutation; persist/remove ops
   are chained per group so a remove can never be overtaken by a stale persist.
   Member ids derive **deterministically from the client deviceId** (UUIDv5) —
   the same device maps to the same member across restarts. On `/socket`
   connect with no in-memory membership, the server recovers membership from
   the snapshot and resyncs the client; a janitor prunes snapshots older than
   48h, never-joined groups dissolve after 120s, and ghost members are pruned
   by a ping-driven TTL (150s, fed by the client's KeepAlive).
2. **Distribution.** With Postgres (ADR-0015), each group has exactly one
   **owning replica**, elected via a per-group **Postgres advisory lock**. The
   other replica holds a *remote handle* that forwards commands over the
   NOTIFY bus; deliveries to members connected to the owning replica bypass
   the bus. Boot reconciliation re-adopts persisted groups with retry rounds
   until ownership is confirmed (a `snapshot()` probe answers only from a
   local actor, which doubles as the ownership test).
3. **Protocol ack rule** (hard-learned, SPEC V21/B9): the server must **never
   withhold the command the client ACKs**. jellyfin-web posts `/SyncPlay/Ready`
   only on an actual player transition — gating Unpause/Seek on Ready while
   withholding the Unpause/Seek is a circular wait. Readiness gates apply only
   to members expected to emit a transition, with a 30s anti-wedge timeout and
   `SetIgnoreWait` honoured.

## Consequences

- A watch party survives process restarts and rolling deploys end-to-end
  (SPEC V25, live-verified); a command from a session with no recoverable
  group answers `NotInGroup` rather than being silently dropped.
- SyncPlay correctness now depends on DB semantics (advisory locks, NOTIFY) —
  on SQLite there is no bus and no lock, which is safe only because SQLite
  deployments are pinned to a single replica (ADR-0015).
- The deterministic member id means a device rejoining is an *update*, not a
  duplicate member — the basis of every recovery path.
- Persistence writes ride group mutations; the per-group op chain bounds the
  race surface but every new mutation site must remember to persist.

## References

- ADR-0014 (the actor + wire protocol this builds on), ADR-0015
- `crates/pharos-sync/` (`group.rs`, `hub.rs`), `crates/pharos-server/src/
  sync_recovery.rs`, `sync_distributed.rs`
- `SPEC.md` §V (V21, V25), §B (B9, B24–B33); memory
  `project_pharos_syncplay_groupwatch`, `project_syncplay_restart_resilience`
