---
description: "Task list for SyncPlay Cast Target"
---

# Tasks: SyncPlay Cast Target

**Input**: `/specs/009-syncplay-cast/` — plan.md, spec.md, research.md, data-model.md, contracts/cast-surface.md, quickstart.md

**Tests**: REQUIRED (constitution III: failing test first, V11; disarm each metric once). Every behaviour task below is preceded by its red test task.

**Organization**: by user story. IDs are feature-local (`T001…`); baseline `T/V/B` ids are cited as `baseline T…` / `V…` to avoid collision. Run everything via `nix develop --command …`; `just test-fast` while iterating, full `just test` before each commit; `just hakari-regen` after any `Cargo.toml` change; clippy before push.

## Format: `[ID] [P?] [Story] Description` — **[P]** = different files, no dependency on an open task.

---

## Phase 1: Setup — instrumentation first (ODD)

**Purpose**: ship the signal before the behaviour so it can be read in production.

- [X] T001 Scaffold crate `crates/pharos-cast` (`Cargo.toml` with workspace lints, `src/lib.rs`), add to workspace `members` in `Cargo.toml`, run `just hakari-regen`
- [X] T002 [P] Add `CastEvent` and `CastReason` enums with stable `label()` methods in `crates/pharos-cast/src/events.rs` (events: `started|rejected|ended|lost|reconnected|handoff_failed`; reasons: `not_in_group|tv_busy|tv_unreachable|user_has_seat|not_permitted|owner_left|group_gone|grace_expired|user_stop|token_failed`) plus a test asserting every label is distinct and bounded
- [X] T003 Add `record_cast_event(event, reason, user, sender_device, tv, group)` in `crates/pharos-cast/src/obs.rs` emitting `pharos_cast_seat_total{event,reason}` and a `tracing` event `cast seat` carrying user, sender device, TV, group and the offending value (never a bare class), on the failure path as well as the success path (test both, analyze F13); register `pharos_cast_drift_seconds` (histogram), `pharos_cast_correction_total{kind}`, `pharos_cast_connection{state}`; test asserts each series via the `metrics` test recorder
- [X] T004 Disarm check: comment out each record call from T003 once, confirm its test fails, restore; note result in the commit body

**Checkpoint**: instrumentation merged on its own commit(s); `cargo nextest run -p pharos-cast` green.

---

## Phase 2: Foundational (blocks every story)

**Purpose**: spikes that decide shapes, then the shared building blocks.

### Spikes (record each outcome in `research.md`; they may change later tasks)

- [X] T005 Spike S1: choose CASTV2 crate vs `rustls`+`prost` (licence vs AGPL-3.0-or-later, async, maintenance); capture the real `PlayNow` / `Seek` / `Pause` / `Unpause` / `Stop` message shapes and receiver progress messages from `jellyfin/jellyfin-chromecast`; confirm TV-token mint path in `crates/pharos-server/src/api/jellyfin/auth_extractor.rs` and the token store, and whether the token store supports per-token scopes (if not, size the smallest addition and give it its own task before T052)
- [ ] T006 [P] Spike S4: in `compat-playwright/`, confirm jellyfin-web 10.11.x lists a server-supplied `GET /Sessions` entry in its device picker; record the minimal field set and fix `supports_remote_control` inconsistency between REST (`sessions.rs:167`) and socket broadcast (`socket.rs ~895-915`) as a failing test in `crates/pharos-server/tests/jellyfin_sessions.rs`
- [ ] T007 [P] Spike S3: in `compat-playwright/`, with two jellyfin-web contexts in a SyncPlay group, send the first context a `PlayState` `Stop` over its socket and record whether it stops locally or turns it into a SyncPlay request

### Group membership kinds (`pharos-sync`)

- [X] T008 Write failing tests in `crates/pharos-sync/tests/group_conformance.rs`: a `CastSeat` member and a `Controller` member never appear in `follower_ids`, never extend a Seek/BufferingUnpause/QueueChange gate, and never receive a buffering freeze aimed at "everyone else" as a wait target (V139, V161); roster lists them
- [X] T009 Add closed enum `MemberKind { Player, Controller, CastSeat }` and optional `cast: {tv, owner}` to the member record in `crates/pharos-sync/src/group.rs`; exclude non-`Player` kinds in `follower_ids` (`group.rs ~1536`) and gate-hold release paths; make T008 green
- [X] T010 [P] Write failing test in `crates/pharos-sync/tests/hub_integration.rs` that a snapshot without `kind` (old format) loads as `Player`, and a snapshot with `CastSeat` round-trips; then add `kind`/`cast` to `PersistMember` with `#[serde(default)]` in `crates/pharos-sync/src/group.rs` (`~2160`)
- [X] T011 Add `GroupMsg::SetMemberKind { member, kind, cast }` handled in the actor, with a `RemoteCommand` variant in `crates/pharos-sync/src/distributed.rs` and bus (de)serialization test in `crates/pharos-sync/tests/distributed_sync.rs`
- [X] T012 Add a non-websocket sink variant in `crates/pharos-sync/src/delivery.rs` (a trait object or enum arm that forwards the group's outgoing `SendCommand`/group updates to a channel); test it receives the same command stream a websocket sink would

### Cast building blocks (`pharos-cast`)

- [X] T013 [P] Define typed connectsdk DTOs from spike S1 in `crates/pharos-cast/src/messages.rs` (command out, report in) with serde round-trip tests against captured fixtures
- [X] T014 [P] Write failing tests then implement the pure planner `plan(target, observed, now, state) -> Action {None|Seek|Pause|Resume}` in `crates/pharos-cast/src/drift.rs` with dead-band, hysteresis and report-age interpolation (the 1500 ms threshold and 5000 ms cooldown are provisional until T048 re-tunes them from the real-TV measurement); add proptests (never two corrections inside the cooldown; never seeks when within threshold; paused target never seeks forward)
- [X] T015 Define `trait CastTransport` (connect, launch, send, recv, close; native `async fn`, no `async_trait`) in `crates/pharos-cast/src/transport.rs`
- [X] T016 Implement the receiver emulator `FakeReceiver` in `crates/pharos-cast/src/fake.rs`: handles the stock command set, mirrors `playbackstart/progress/stop` every 1.5 s, injectable latency, drift, drop-connection and seek-inaccuracy; unit tests for each knob
- [ ] T017 Implement the real CASTV2 `CastTransport` in `crates/pharos-cast/src/transport.rs` per spike S1 (TLS 8009, heartbeat, launch `F007D354`), with connect timeout and retry with backoff; integration test against the emulator exposed over a local TLS socket if the chosen crate allows, else gated behind a `real-tv` feature
- [X] T018 [P] Add `[cast]` / `[[cast.tvs]]` config in `crates/pharos-server/src/config.rs` per `contracts/cast-surface.md` with validation errors naming the entry and value; test valid, duplicate-name and bad-address cases
- [X] T019 Add TV-token mint and revoke in `crates/pharos-server/src/cast.rs` (device-bound, bounded lifetime, hashed at rest, id returned separately from secret); tests: bounded lifetime, revoked token fails auth, secret absent from `Debug` and logs (V8)

- [X] T057 [P] Failing test then fix: leader election in `crates/pharos-sync/src/group.rs` (`elect_leader`, `remove_member`) never picks a `Controller` or `CastSeat` while a `Player` is connected, so leader-only commands and handoff Pause stay answerable (found in review of T009)
- [X] T058 [P] Failing test then fix: `MemberJoined`/`MemberLeft` broadcasts do not announce a hidden `CastSeat` as a separate member when its paired `Controller` exists, consistent with the collapsed roster (`crates/pharos-sync/src/group.rs`, review of T051)
- [ ] T059 [US1] When the hand-off re-classifies a freshly joined member via `SetMemberKind`, the group must tell the other members (a `MemberLeft` for the now-hidden seat or a refreshed roster) because the seat was announced as a Player at join time; found in review of T058, implement in the hand-off path (`crates/pharos-server/src/cast.rs`, `crates/pharos-sync/src/group.rs`) with a test that other members' roster never keeps a stale seat entry
- [ ] T049 Checkpoint after T005–T007: re-read plan.md, tasks.md and `contracts/cast-surface.md` against the spike outcomes, amend them, and re-run `/speckit-analyze` before T013/T017/T026/T028 proceed (analyze F10)

**Checkpoint**: group kinds, planner, transport trait, emulator, config and token mint exist and are tested; no behaviour exposed yet.

---

## Phase 3: User Story 1 — Cast my seat in a group to a TV (P1) 🎯 MVP

**Goal**: member picks the TV; the TV plays at the group position and follows the group; owner appears once.

**Independent Test**: two members in a group (one test client, one emulator TV); play/pause/seek from the non-cast member lands on the emulator within SC-001, and the roster shows the owner once.

### Tests first

- [ ] T020 [P] [US1] Failing test `crates/pharos-server/tests/cast_virtual_session.rs`: `GET /Sessions` lists a configured, reachable, permitted TV with `SupportsRemoteControl: true`, `Client: "Chromecast"`; not listed for a non-permitted user (V9) or an unreachable TV; same values on the socket `Sessions` broadcast
- [ ] T021 [P] [US1] Failing test `crates/pharos-server/tests/cast_start.rs`: `POST /Sessions/{tv}/Playing` by a group member creates a seat, the emulator receives `PlayNow` at the group position, the owner's member becomes `Controller`, the roster shows the owner once with the TV as device; non-virtual ids behave exactly as before
- [ ] T022 [P] [US1] Failing test in `crates/pharos-sync/tests/group_conformance.rs`: group pause/unpause/seek reach the cast sink as the expected cast commands, and a TV that never acks does not extend any gate
- [ ] T023 [P] [US1] Failing test in `crates/pharos-cast/src/driver.rs` (module tests): `CastDriver` applies a group Seek to the emulator, then corrects injected drift via the planner, records `pharos_cast_correction_total`

- [ ] T050 [P] [US1] (roster test done in 26e279d2; `tv_busy`, slow-Loading and stable-session-id tests remain) Failing tests: the roster shows a casting user once (TV as device) in `crates/pharos-sync/tests/group_conformance.rs`; a `tv_busy` rejection, a slow-Loading start within the freeze bound, and a decode-failure end with a reason in `crates/pharos-server/tests/cast_start.rs`; virtual session id is stable across restart in `cast_virtual_session.rs` (analyze F1, F6, F11)
- [X] T051 [US1] Implement the one-entry roster projection in `crates/pharos-sync/src/group.rs` (group-update/roster builder) so T050 passes
- [ ] T052 [US1] Scope the TV token to playback endpoints only (analyze F8). T005 found tokens carry NO scope field, so this needs: a nullable `scope` column on `auth_tokens` (sqlite + postgres migrations, `just test-postgres` afterwards), `TokenStore::issue_with_ttl` taking a scope, the scope returned by `resolve`, and a check in the `AuthUser`/`AuthSession` extractors (`crates/pharos-server/src/api/jellyfin/auth_extractor.rs`) that refuses a `Playback`-scoped token on admin and library-mutation routes; plus a test that proves the refusal through the real extractor. Replace the `// T052:` comment at the mint site in `crates/pharos-server/src/cast.rs`

### Implementation

- [ ] T024 [US1] Implement `CastDriver` actor (state machine from `data-model.md`: Connecting → Loading → Following) in `crates/pharos-cast/src/driver.rs`: owns one `CastTransport`, consumes group commands from the cast sink, feeds receiver reports to the planner, emits `record_cast_event`
- [ ] T025 [US1] Implement the seat registry and hand-off in `crates/pharos-server/src/cast.rs`: validate caller is a group member and not already a seat owner and TV not busy (rejections recorded with reasons), mint token, spawn `CastDriver`, `SetMemberKind` (owner → `Controller`, new seat member `CastSeat` keyed `UUIDv5("{user}:cast:{tv}")`), launch + `PlayNow` at the group's current position and item
- [ ] T026 [US1] Project virtual TV sessions into `GET /Sessions` and the socket `Sessions` broadcast in `crates/pharos-server/src/api/jellyfin/sessions.rs` and `crates/pharos-server/src/api/jellyfin/socket.rs` as a typed DTO (V38), same values both places
- [ ] T027 [US1] Intercept `POST /Sessions/{id}/Playing` for virtual TV ids in `crates/pharos-server/src/api/jellyfin/sessions.rs`: start via T025, reject with the Jellyfin error shape and stable reason string otherwise
- [ ] T028 [US1] Send the owner's socket a `PlayState` `Stop` on hand-off in `crates/pharos-server/src/cast.rs` (behaviour per spike S3; if S3 shows it is ignored, implement the fallback in research R8 and update the test in T021)
- [ ] T029 [US1] Wire the `CastDriver` registry into `main`/`AppState` startup in `crates/pharos-server/src/state.rs` and `lib.rs` (registry as an actor handle, no `Mutex<State>` on the request path, V18)

**Checkpoint**: US1 works against the emulator; MVP.

---

## Phase 4: User Story 2 — Drive the group from the sending device (P1)

**Goal**: commands a client addresses to the TV act on the group; queue changes move the TV.

**Independent Test**: from the owner's client send Pause/Seek/Next addressed to the TV session; every member including the TV follows; nothing is applied to the TV alone.

- [ ] T030 [P] [US2] Failing test `crates/pharos-server/tests/cast_commands.rs`: `POST /Sessions/{tv}/Playing/Pause|Unpause|Seek|NextTrack|PreviousTrack` produce the matching group command and no raw `PlayState` broadcast to the TV; `Stop` from the owner ends the cast (V169)
- [ ] T031 [P] [US2] Failing test in the same file: a queue change by another member (`SetNewQueue`/`NextItem`) makes the TV load the new item at the right position via `CastDriver`
- [ ] T032 [US2] Intercept `POST /Sessions/{id}/Playing/{command}` for virtual TV ids in `crates/pharos-server/src/api/jellyfin/sessions.rs`, translating to the existing SyncPlay dispatch in `crates/pharos-server/src/api/jellyfin/syncplay.rs` (`dispatch`) as the owner; ignore volume/display `Command/{command}` for TV ids with a debug log
- [ ] T033 [US2] Handle queue/item change in `CastDriver` (`crates/pharos-cast/src/driver.rs`): load the new item via the connectsdk command from spike S1 and re-enter Loading, bounded by the same freeze bound as any member
- [ ] T034 [US2] Per-member trace: every command routed through a virtual TV id records `pharos_syncplay_command_total` with its own outcome (V160) — add test

- [ ] T053 [P] [US2] Failing test then implementation: a pause/seek/stop reported by the receiver itself (emulator-injected, not caused by our command) is applied to the group as an owner command, and our own corrections are not echoed back (debounce by command id), in `crates/pharos-cast/src/driver.rs` and `crates/pharos-server/tests/cast_commands.rs` (analyze F2)
- [ ] T054 [P] [US2] Failing test then implementation: the driver holds each group command until its scheduled apply time minus measured cast latency (`crates/pharos-cast/src/driver.rs`; analyze F4)

**Checkpoint**: US1 + US2 usable end to end against the emulator.

---

## Phase 5: User Story 3 — Stop casting without leaving the group (P2)

**Goal**: stop, TV loss and group end clean up; the owner returns to `Player` at the group position.

**Independent Test**: start a cast, then (a) owner stops, (b) emulator drops its connection, (c) emulator reconnects inside grace, (d) group dissolves — assert group disruption stays within the bounded freeze and the owner is restored.

- [ ] T035 [P] [US3] Failing tests `crates/pharos-server/tests/cast_end.rs` for (a)–(d) above, each asserting the `pharos_cast_seat_total{event,reason}` row, token revoked (V171), and other members never frozen beyond the existing bound
- [ ] T036 [P] [US3] Failing test: `POST /SyncPlay/Leave` by a seat owner ends the seat first (reason `owner_left`); a second cast start by a user who already owns a seat is rejected `user_has_seat`
- [ ] T037 [US3] Implement Lost/reconnect/Ended in `CastDriver` (`crates/pharos-cast/src/driver.rs`): connection loss marks the seat disconnected (reusing `MemberSocketLost` semantics so gate holds release), reconnect with backoff inside `reconnect_grace_secs`, then Ended (`grace_expired`)
- [ ] T038 [US3] Implement end/restore in `crates/pharos-server/src/cast.rs`: revoke token, remove the seat member, restore the owner to `Player`, resync the owner through the existing late-joiner path (`ResyncMember`), stop the Cast session on the TV, record the event
- [ ] T039 [US3] Hook `leave_group` and group dissolve in `crates/pharos-server/src/api/jellyfin/syncplay.rs` and `crates/pharos-sync/src/registry.rs` to call end/restore

- [ ] T055 [P] [US3] Failing test: the owner's socket drops (sender backgrounded/closed) while casting; the seat keeps following and the owner's controller entry survives the existing reconnect grace; add to `crates/pharos-server/tests/cast_end.rs` (analyze F5)
- [ ] T056 [US3] On restore send the owner's client a play-at-group-position message after the resync so its local player restarts; test in `crates/pharos-server/tests/cast_end.rs` using the test client, behaviour per spike S3 (analyze F3)

**Checkpoint**: all P1/P2 stories hold under failure.

---

## Phase 6: User Story 4 — Cast first, join afterwards (P3)

**Goal**: order of casting and joining does not matter.

**Independent Test**: with no group, a play-on-TV request is refused with a readable reason; after joining a group, the same request succeeds and the TV adopts the group position.

- [ ] T040 [P] [US4] Failing test `crates/pharos-server/tests/cast_order.rs`: not-in-group request → `400` reason `not_in_group` and a `rejected` event (FR-009); after `POST /SyncPlay/Join` the same request succeeds; leaving the group while casting ends the seat
- [ ] T041 [US4] Make T040 green with the existing code paths; add only what is missing in `crates/pharos-server/src/cast.rs`

---

## Phase 7: Persistence, deploy resilience, polish

- [ ] T042 [P] Failing test `crates/pharos-server/tests/cast_restart_recovery.rs` (template: `syncplay_restart_recovery.rs`): a persisted `CastSeat` member is re-created after restart, the connection is re-established, the TV returns to `Following` within the V25 bound; then implement hydrate in `crates/pharos-server/src/sync_recovery.rs` and `crates/pharos-server/src/cast.rs`
- [ ] T043 Single-owner TV connection under multi-replica: only the group-owning replica holds the Cast connection; failover hands it over (`crates/pharos-sync/src/distributed.rs`, `crates/pharos-server/src/sync_distributed.rs`); test in `crates/pharos-sync/tests/distributed_sync.rs`
- [ ] T044 [P] Extend `crates/pharos-jellyfin-test-client/src/lib.rs` with socket, SyncPlay and session-command helpers needed by T021/T030/T035 (if not already added)
- [ ] T045 [P] Playwright spec `compat-playwright/` + `just compat-syncplay` wiring: two jellyfin-web contexts + `FakeReceiver` as the TV (picker lists TV, hand-off, pause/seek follow)
- [ ] T046 Append invariants V168–V171 to `specs/001-pharos-baseline/invariants.md`; add baseline `T129` (cast seat) to `specs/001-pharos-baseline/tasks.md`; update spec 009 Status and `research.md` with spike results
- [ ] T047 Run `just hakari-check`, clippy (`cargo clippy --workspace --all-targets -- -D warnings` in the devShell) and the full `just test`
- [ ] T048 Manual real-TV run per `quickstart.md` §3; run the §4 queries and paste the output into `quickstart.md`. Measurement: SC-001 is command-to-apply on the emulator (CI) and, on the real TV, position error against the group clock from interpolated receiver reports; SC-002 is that position error sampled over a full item. Re-tune the `[cast]` threshold/cooldown defaults from the result. Until done, SC-001/SC-002 are reported as **unwitnessed**. If S2 shows 2 s is infeasible, restate SC-002 from the measurement in `spec.md`

---

## Dependencies & Execution Order

- Task ids are append-only: T049–T056 were added after analysis and sit inside their phases; execution order is the file order, not id order (T050–T052 run before T024).

- Phase 1 → Phase 2 → stories. Spikes T005–T007 gate T013, T017, T028 and may reshape T021/T033.
- Within Phase 2: T008→T009; T010 and T011 after T009; T012 after T009; T013–T016, T018 parallel; T017 after T005, T015; T019 after T005.
- US1 (Phase 3) is the MVP and gates US2/US3/US4. US2 and US3 can proceed in parallel once US1 is green (different files: `sessions.rs` command arm vs `driver.rs` Lost/Ended + `cast.rs` end path), though both touch `cast.rs` so merge sequentially. US4 needs only US1.
- Phase 7 after the stories; T048 last.

### Parallel examples

- Phase 2: T013, T014, T016, T018 together.
- US1 tests: T020, T021, T022, T023 together (four different files).
- US2 tests T030/T031 together; US3 tests T035/T036 together.

## Implementation Strategy

1. Merge Phase 1 alone (instrumentation) so the signal is readable first.
2. Do the spikes, update `research.md`, then Phase 2.
3. Ship US1 behind the config block (no `[[cast.tvs]]` = feature inert), prove it on the emulator and a real TV (T048 early if a TV is at hand).
4. Add US2, then US3, then US4; each as its own atomic commits (one logical change each, never squashed, per the git mandate).
5. Postgres: no `sqlx::query*` strings are touched (snapshots are JSON in an existing column), so `just test-postgres` is not required unless T042 changes persistence queries.
