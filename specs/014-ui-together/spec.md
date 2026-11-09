# Feature Specification: watch together, watch a URL, control other screens

**Feature directory**: `specs/014-ui-together/`
**Created**: 2026-11-01
**Status**: implemented on main (US1, US6-US10, US2 partial without chat/reactions or the group queue: native Group/Membership, Together page, Playwright two-browser + mixed jellyfin-web); open: T027 (stub cleanup), T029 (10-minute mixed drift run); US3-US5 not tracked in tasks.md (audited 2026-11-09)
**Input**: "… all the things Jellyfin's UI does and more (tailored to the Pharos
server)." (Alison, 2026-11-01)
**Depends on**: 009 (websocket FR-004), 013 (player, queue), ADR-0014/0016
(group sync), `docs/group-sync-protocol.md`, 008 (remote sources)
**API**: pharos-ui calls the pharos-native API (017, ADR-0028). Jellyfin
endpoints named below identify the existing server behaviour that a native
method wraps through a shared domain service (V172); they are not what the UI
calls. Jellyfin clients keep using them unchanged.
**Supersedes**: T27 phase 2, T49 phase 2, T60; the 155-line unmounted
`views/group.rs` stub.

## Goal

The thing pharos is best at — sitting down with friends and watching anything
together — is a first-class part of the UI, not a menu entry.

## User Stories

### US1 — start or join a group (P1)

From any item: "Watch together" creates a group and starts it; others join from
a Together page listing active groups, from an invite link (`/together/{group}`,
009 FR-001), or from a toast when a friend starts one.

**Independent test**: two Playwright browsers; A creates, B opens invite link,
both play in sync (reuse `compat-playwright/tests/lib/sync-oracle.ts`).

### US2 — in-group experience (P1)

Member strip (avatars, who is buffering, who is leader/host), drift indicator
per member, group queue (add, reorder, vote-skip `[NEEDS CLARIFICATION]`),
chat with emoji reactions overlaid on the video (fade out), "wait for
everyone" vs "keep going" on buffering, leave/disband.

Chat and reactions travel on the native `Group` interface (017), not the
Jellyfin socket; Jellyfin SyncPlay members in a mixed group do not see them.

### US3 — remote control other sessions (P1)

A Devices/Sessions panel lists the user's other active sessions (TV, phone,
other browser); pick one to play the current item there, or control it
(play/pause/seek/volume/next/subtitles) with the 011 player keys. Replaces
jellyfin-web's cast bar for pharos sessions.

**Endpoints**: `GET Sessions`, `POST Sessions/{id}/Playing`,
`/Playing/{command}`, `/Command`.

### US4 — watch a URL (P2, pharos-only)

Paste a URL (palette or home tile); the UI calls the 008 remote-items endpoint,
shows resolve progress and failure reason, then the result behaves like any
item: play, add to group queue. Hidden for users without the permission or when
`[remote].enabled = false`.

### US5 — group-aware everywhere (P2)

Cards show a "watching together now" badge when a group is on that item;
detail page offers "Join group"; mini-player shows group state.

### US6 — who has seen what (P2)

In a group, a film, episode, season or series page shows an "In this group"
row: each member with seen, part-way with a percent, or not seen, and for a
series or season "6/10 episodes". Each episode row of a season shows who has
seen it. Hovering a member on the watch page's strip shows whether they have
seen the item playing. It answers "who needs catching up?" and "who has seen
this film?". A member's state is shown only to others in the same group, and
only for items the viewer may see (ADR-0054, V365).

### US7 — the group's audio and subtitles, with a personal override (P1)

Alison, 2026-11-04: "sync around the subtitles and audio track switching ...
with the ability for people to override". A group has one audio track and one
subtitle choice (a track or off). When a member changes either from the
player's menu, every member who follows the group switches with them. A member
can tick "Just me" in the menu: from then on their own picks stay theirs and
the group's changes pass them by, until they tick "Follow group" again, which
puts them back on the group's tracks. A late joiner starts on the group's
tracks. The member strip marks a member on their own tracks. Moving the group
to another item clears its choice (the new item's tracks differ). jellyfin-web
members are not affected; their SyncPlay has no track sync (V396).

### US8 — what is going on in the group (P1)

Alison, 2026-11-04: "the member list to show if someone is buffering and who
paused, seeked or whatever so people in the group know what is going on". The
member strip shows each member's state (buffering, on their own tracks), and a
group play, pause, seek or track change names who did it ("Sam paused at
12:04", "Ali skipped to 40:10", "Waiting for Sam"). The newest line also shows
over the video for a few seconds even while the player's controls are hidden,
and the strip keeps the last few. A change the group makes on its own (everyone
ready, so play starts) says so instead of naming anyone (V395).

### US9 — the group moves to the next episode together (P1)

Alison, 2026-11-07: test "skipping back and forward episodes" and "swapping
content" in a group, then "Yes add that please" to the group moving with them.
A member who presses the player's next or previous episode, or whose player
reaches the end of an episode with another after it, moves the whole group:
every member's player opens that episode and they play it together from its
start. When both players reach the end at once, the group moves once: the
second move names the entry the first one replaced and is dropped as stale.
The group moves only to an item every member may see, as a queue change
(V201); otherwise, or on a server too old for the move (schemaVersion below
23), the member who pressed it plays it on their own and stays a member. A
member watching another item on their own (V508) moves only themself (V611).

### US10 — the group swaps to another title (P1)

Alison, 2026-11-07: test "swapping content" in a group. In a group, a film or
episode's title page offers "Play for the group" ("Resume for the group" and
"Play for the group from start" for a part-watched title, from the viewer's
place as for Watch together) in place of Watch together. It moves the whole
group to that title, as a next episode does (US9, V611), even from a member
watching something else on their own; before this, Watch together there left
the group and started a new one with nobody else in it. Play, and any song,
still plays for that member alone (V508). A member whose film is shrunk to the
floating player plays the new title there, as any play does (spec 020 FR-001)
(V612).

## Functional Requirements

- **FR-001** Group-sync client implementing `docs/group-sync-protocol.md` over
  the native `Group` interface (017). The server bridges groups so pharos-ui and
  jellyfin-web/native Jellyfin members can share one group (V20).
- **FR-002** Clock sync (`/GetUtcTime`-style ping) and drift correction via
  playback-rate nudge before hard seek.
- **FR-003** Sessions/remote-control client per US3.
- **FR-004** Remote-source submit + status per US4, error messages naming the
  resolver outcome (`pharos_remote_resolve_total{outcome}` labels).
- **FR-005** Instrumentation: drift samples, hard-seek count and buffering waits
  per group are reported (ODD: name the signal for "group feels out of sync").

## Success criteria

- **SC-001** Mixed group (pharos-ui + jellyfin-web) stays within the protocol's
  drift bound for 10 min in `just compat-syncplay`.
- **SC-002** Remote control of a jellyfin-web session from pharos-ui:
  play/pause/seek verified on the target.
- **SC-003** Watch-a-URL plays end to end against the 008 integration fixture.

## Edge cases

- Leader leaves; network drop and rejoin; a member whose device must transcode
  while others direct-play (different start latency).
- Stale group id in an invite link → "group ended", not a new empty group
  (relates to T90a).

## Progress (audited 2026-11-09)

- US1 start or join a group: done (tasks T001-T013, T043, T049-T051; native Group/Membership in `crates/pharos-server/src/api/pharos/rpc/group.rs`, UI in `crates/pharos-ui/src/native/together*.rs`).
- US2 in-group experience: partial (T014, T024, T025, T026, T041; chat and reactions deferred to their own plan, not in the native `Group` interface; the group queue (members add and reorder items, vote-skip) is also deferred per plan.md and is remaining P1 work; the member strip shows names, buffering, seen marks, own-track indicators and a recent-activity log (`together_view.rs` `MemberStrip`), but no per-member avatar or drift indicator (`together.rs` `Member` has no such fields)).
- US3 remote control other sessions: not started on the native API (no remote-control interface in `pharos.capnp`; only the legacy `views/remote.rs` stub over `/Sessions/...`); no task exists.
- US4 watch a URL: not started in pharos-ui (no remote-items call in `crates/pharos-ui/src/native`); no task exists.
- US5 group-aware everywhere: partial (Together page lists groups; no card badge / "Join group" on detail verified); no task exists.
- US6-US10: done (T030-T051).
- Open tasks: T027 (stub `views/group.rs` still present), T029 (10-minute mixed-group drift run in `just compat-syncplay`).
