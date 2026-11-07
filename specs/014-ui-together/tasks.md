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
compiles, passes `just test-changed`, and reverts alone; the full `just test`
runs before each push (CLAUDE.md).

## Phase 1: shared dispatch (server, no behaviour change)

- [ ] T001 Move `dispatch_checked`, `recover_and_apply`, the `SyncPlayAccess`
  guard and the V201 visibility checks (`may_join`, `may_see_queue`,
  `may_queue`, with their `pharos_syncplay_group_hidden_total` counting) out of `crates/pharos-server/src/api/jellyfin/syncplay.rs` into a
  surface-neutral module (`crates/pharos-server/src/group_ops.rs`) taking a
  member key instead of a Jellyfin request; Jellyfin handlers call it. Existing
  syncplay tests stay green unchanged.

## Phase 2: native `Group` interface (server)

- [ ] T010 Schema: append `Session.group`, `Group`, `Membership`, `GroupSink`,
  `GroupUpdate`, the result structs and the three `EventKind` additions from
  plan.md §Contract (with the kind→label match in `rpc/telemetry.rs` and 016
  `contracts/events.md`) to
  `crates/pharos-api/schema/pharos.capnp` and its copy in
  `specs/017-native-api/contracts/`; bump `schemaVersion` to 6. Schema-compat
  test (017 FR-009) passes.
- [ ] T011 [US1] `crates/pharos-server/src/api/pharos/rpc/group.rs`: `list`
  and `clock`, plus the `Session.group` getter. `it_rpc/group.rs`: list shows
  a group a Jellyfin client made; `clock` answers the server time; a user
  without `SyncPlayAccess` is refused `notPermitted`; a user who may not see
  an item in a group's queue does not see the group in `list` and cannot
  `join` it, and `pharos_syncplay_group_hidden_total` counts both (V201).
- [ ] T012 [US1] `create` and `join`: register the member in `SessionHub`
  under `ctx.device`, pump its `ServerMsg` stream into the `GroupSink`; no
  device answers `invalid{ctx.device}`. Test: native A creates, native B joins,
  B's sink sees `joined` then A as a member.
- [ ] T013 [US1] `Membership` commands map to `GroupMsg` through T001's path.
  Tests: A pauses, B's sink gets `pause` with A's position; a Jellyfin member
  in the same group gets a `SyncPlayCommand` for it (V20); `leave` ends with
  `ended{left}`; joining a dissolved group answers `notFound`, never a new
  group (spec edge case). `ping` refreshes the member past the 150 s ghost
  TTL; `clockReport` reaches `ObserveClock`; a stale `playlistItemId` on
  `ready` is ignored as for a Jellyfin member (B37).
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
- [x] T025 FR-005: the group store reports `groupDrift`, `groupHardSeek` and
  `groupWait` (kinds added in T010); test names the disarm.
- [x] T026 [US1] Two-browser Playwright test on `sync-oracle.ts`: A creates,
  B opens the invite link, both play in sync, A pauses and B pauses, A seeks
  and B converges. The player's `<video>` carries `data-item-id`.
- [ ] T028 [US1] Mixed group in Playwright: a pharos-ui browser and a
  jellyfin-web browser in one group (either creates), held in sync by
  `sync-oracle.ts` through play, pause and seek (V20).
- [ ] T029 SC-001: `just compat-syncplay` gains a pharos-ui member and holds
  the protocol's drift bound for 10 minutes in a mixed group.
- [ ] T027 Spec ids: record the native group path in
  `specs/001-pharos-baseline/` (bug or invariant as the work finds), and mark
  the 155-line `views/group.rs` stub superseded or remove it.
- [x] T030 [US6] Contract: `Membership.seen`, `SeenResult`, `MemberSeen`,
  `ItemSeen`; `schemaVersion` 11 → 12 in both capnp files.
- [x] T031 [US6] Server: `services::group_seen` and `Membership.seen` for the
  caller's own group, V201-bounded, capped at 200 ids, with
  `pharos_group_seen_total{outcome}`; ADR-0054. `it_rpc` tests: B sees A's
  played episode, series and season counts, an invisible item omitted, refused
  outside the group and over the cap.
- [x] T032 [US6] `GroupSnapshot.participant_users` so a participant's user is
  not read from the unordered `member_users` (B425).
- [x] T033 [US6] `pharos-client` `NativeMembership::seen`.
- [x] T034 [US6] UI: `GroupClient::seen`, `Session::seen`, the `seen` marks,
  "In this group" on title and show pages, dots on episode rows, a mark on the
  watch page's member strip; unit and render tests.
- [x] T035 [US6] Two-browser Playwright: A marks an episode played and B sees it
  under "In this group".
- [x] T036 [US7][US8] Contract: `TrackChoice`, `GroupTracks`, `GroupMember.ownTracks`,
  `GroupSummary.tracks`, `Membership.setTracks`/`followTracks`,
  `GroupUpdate.tracks`, `At.by`; one `schemaVersion` bump in both capnp files.
- [x] T037 [US8] Server: `ServerMsg::{Play, Pause, Seek}` carry the member that
  caused them; native `At.by` from it; V395. `it_rpc`: A pauses, B's sink sees
  `pause` by A; a ready-gate start has no `by`.
- [x] T038 [US7] Server: group tracks and own-tracks in the actor, `setTracks`,
  `followTracks`, `tracks` and member updates, reset on a queue change, late
  joiners get them in `joined`; `pharos_group_tracks_total{outcome}`; V396; ADR.
- [x] T039 [US7] `pharos-client` wrappers for `setTracks`, `followTracks` and
  the new update fields.
- [x] T040 [US7] UI: a following member applies group tracks; the audio and
  subtitle menus send `setTracks`, or change only the local player under
  "Just me"; "Follow group" re-applies; the strip marks own-tracks members.
- [x] T041 [US8] UI: the `activity` lines in the member strip and over the
  video; buffering shown per member.
- [x] T042 [US7][US8] Two-browser Playwright: B sees "A paused"; A picks a
  subtitle and B follows; B ticks "Just me", A changes again, B stays.
- [x] T043 [US1] UI: start a group from the player mid-watch at the video's
  position, with an Invite button and panel for the group's link; V479.
  Two-browser Playwright: A watches, starts the group, B joins by the link.
- [x] T044 [US1][US7] Two-browser Playwright as users opted in to diagnostics
  (seeded together-a, together-b), each case failing on an error-level client
  event: seek forward and back by either member, playing and paused; the
  group's audio pick and back (B660); the group events (`groupWait`,
  `groupDrift`, `groupHardSeek`) counted in `/metrics`, and none from a member
  who has not opted in. `playwright.together.config.ts` runs them in Chromium
  and Firefox.
- [x] T045 [US9] Server: `Membership.setItem` (schemaVersion 23), the group
  actor's `MoveToItem` (a stale `from` entry dropped, `may_queue` checked),
  `pharos_syncplay_move_to_item_total{outcome}`; `it_rpc` cases; V611.
- [x] T046 [US9] `pharos-client` `NativeMembership::set_item`; UI: next and
  previous episode and the end of an episode call `Session::move_to`, and a
  refused move plays the episode alone.
- [x] T047 [US9] Two-browser Playwright: A's next episode and B's previous
  episode move both members; an episode's end moves the group once.
- [x] T048 [US10] UI: in a group, the title page's together buttons read
  "Play for the group" and call `Session::play_for_group`, which moves the
  group from any page; two-browser Playwright: A opens another film's title
  and plays it for the group, both members play it in step; V612.
- [x] T049 [US1] UI: a member says ready only at the group's position: a
  joiner whose player loaded at their own resume point is seeked to the group
  first (B664, V615). Two-browser Playwright: B, with a resume point 72 s in,
  joins a paused group and waits at its position.
- [x] T050 [US1] UI: a reloaded tab joins its group again from the id it kept
  in sessionStorage (B665, V616). Two-browser Playwright: B reloads mid-film
  and is back in the group, in step.
