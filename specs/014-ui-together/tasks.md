---
description: "Task list for watch together in pharos-ui (MVP)"
---

# Tasks: watch together in pharos-ui (MVP)

**Input**: [spec.md](./spec.md), [plan.md](./plan.md)

**Tests**: INCLUDED (constitution III, test-first). Each observability test
names its disarm step and goes red with the instrumentation removed.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelisable, different files, no dependency on an open task
- **[Story]**: US1, US2 from spec.md

Every command runs inside the devShell. Each task closes one atomic commit that
compiles, passes `just test`, and reverts alone.

## Phase 1: shared dispatch (server, no behaviour change)

- [ ] T001 Move `dispatch_checked`, `recover_and_apply` and the `SyncPlayAccess`
  guard out of `crates/pharos-server/src/api/jellyfin/syncplay.rs` into a
  surface-neutral module (`crates/pharos-server/src/group_ops.rs`) taking a
  member key instead of a Jellyfin request; Jellyfin handlers call it. Existing
  syncplay tests stay green unchanged.

## Phase 2: native `Group` interface (server)

- [ ] T010 Schema: append `Session.group`, `Group`, `Membership`, `GroupSink`,
  `GroupUpdate` and the result structs from plan.md §Contract to
  `crates/pharos-api/schema/pharos.capnp` and its copy in
  `specs/017-native-api/contracts/`; bump `schemaVersion` to 6. Schema-compat
  test (017 FR-009) passes.
- [ ] T011 [US1] `crates/pharos-server/src/api/pharos/rpc/group.rs`: `list`
  and `clock`, plus the `Session.group` getter. `it_rpc/group.rs`: list shows
  a group a Jellyfin client made; `clock` answers the server time; a user
  without `SyncPlayAccess` is refused `notPermitted` (V201).
- [ ] T012 [US1] `create` and `join`: register the member in `SessionHub`
  under `ctx.device`, pump its `ServerMsg` stream into the `GroupSink`; no
  device answers `invalid{ctx.device}`. Test: native A creates, native B joins,
  B's sink sees `joined` then A as a member.
- [ ] T013 [US1] `Membership` commands map to `GroupMsg` through T001's path.
  Tests: A pauses, B's sink gets `pause` with A's position; a Jellyfin member
  in the same group gets a `SyncPlayCommand` for it (V20); `leave` ends with
  `ended{left}`; joining a dissolved group answers `notFound`, never a new
  group (spec edge case).
- [ ] T014 [US2] Buffering and member updates reach the sink (`member` with
  `buffering`), and dropping `Membership` is a socket loss, not a leave (V65).

## Phase 3: pharos-ui

- [ ] T020 [P] `pharos-client` wrappers for `Group` and `Membership`, and a
  `GroupSink` server whose updates land on a channel.
- [ ] T021 [P] Clock sync: median-of-9 offset from `clock` round trips; pure
  function with unit tests.
- [ ] T022 Drift policy (plan.md clarification 8) as a pure function from
  expected and actual position to `Hold | Rate(f64) | Seek(ms)`, unit tested
  at each threshold.
- [ ] T023 [US1] "Watch together" on the detail page creates a group and opens
  the watch page in group mode; `/together` lists groups; `/together/{id}`
  joins, or shows "This group has ended".
- [ ] T024 [US2] Member strip on the watch page overlay: names, who is
  buffering, a Leave button. Player controls send group commands while joined.
- [ ] T025 FR-005: `groupDrift`, `groupHardSeek`, `groupWait` telemetry kinds
  (append to `EventKind`), reported by the group store; test names the disarm.
- [ ] T026 [US1] Two-browser Playwright test on `sync-oracle.ts`: A creates,
  B opens the invite link, both play in sync, A pauses and B pauses, A seeks
  and B converges. The player's `<video>` carries `data-item-id`.
- [ ] T027 Spec ids: record the native group path in
  `specs/001-pharos-baseline/` (bug or invariant as the work finds), and mark
  the 155-line `views/group.rs` stub superseded or remove it.
