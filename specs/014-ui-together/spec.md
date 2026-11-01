# Feature Specification: watch together, watch a URL, control other screens

**Feature directory**: `specs/014-ui-together/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
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
