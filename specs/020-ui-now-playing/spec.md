# Feature specification: playback that follows you around pharos-ui

**Feature directory**: `specs/020-ui-now-playing/`
**Created**: 2026-11-03T22:10:00Z
**Status**: draft, awaiting Alison's review of the plan
**Input**: "The player should enable us to explore the library while playback
continues, for example we might want to go explore the music library while
listening to music still to queue more tracks into a queue or maybe make a
playlist." (Alison, 2026-11-03)
**Depends on**: 009 FR-008 (shell, mini-player slot), 013 US2/US7, FR-007,
FR-009, SC-004 (the requirements this spec delivers), 017 (native API, the
reserved `Queue` capability), #590 (now-playing screen with its own controls:
`ListenView { media, on_control }`), V235 (full-viewport watch page), V236
(a new play session mounts a new player)
**Delivers**: 013 US7 (mini-player and queue, minus lyrics, waveform and
gapless), 013 SC-004
**Amends**: 013 FR-007 ("persisted per session"): v1 persists the queue on the
device, not on the server; see Decisions
**Follow-up (not built here)**: playlists in pharos-ui (012 US9) as spec 021

## Problem

Playback lives inside the watch route. `WatchScreen` owns the `Player`
component, the engine and the progress reporter, and `Screen { route }` swaps
it out on every navigation, so `use_drop` detaches the engine and reports
Stopped. Leaving a song to look for the next one ends the song. There is no
queue: a track plays alone, and the only "next" anywhere is the episode
neighbour lookup on the watch page (B292).

Today, for someone listening to music:

1. Open an album, play track 3.
2. Press Back to look at the album again: the music stops.
3. There is no way to line up track 7 or a song from another album.

## What already exists server-side

- **Playlists** (T70) are fully stored: `PlaylistStore` (`pharos-core`
  `lib.rs`, create, list per owner, ordered entries with a stable
  `PlaylistItemId`, add, remove, move) behind Jellyfin `/Playlists/*`. The
  native API has no playlist methods yet, and `Library.query`/`detail` for
  playlist ids are open tasks 017 T061/T062. Spec 021 builds on this store.
- **Play queue**: Jellyfin has no server-side play queue for a single user.
  Clients send `NowPlayingQueue` on progress reports, which pharos ignores,
  and the only server queue is the SyncPlay group queue (014,
  `PlayQueueUpdate`; queue ops in progress on `feat/syncplay-queue-ops`).
  The native schema reserves a `Queue` capability on `Session` (017
  data-model) but defines nothing yet.

So v1's queue is client-side; the server is unchanged except one optional
append-only schema field (US2).

## User stories

### US1 — music keeps playing while I browse (P1)

Start a song, then go anywhere in the app except Manage: the song keeps
playing and a now-playing bar sits at the bottom of the shell with artwork,
title, artist, play/pause, previous, next, a thin progress line and a button
that opens the full now-playing screen (#590's `ListenView`). The bar sits
above the content (the page reserves its height) and on narrow screens above
the bottom navigation. Closing the bar (×) stops playback and reports
Stopped. Opening the now-playing screen and leaving it again never restarts
the stream.

**Independent test**: Playwright plays the seeded "carefree" song, goes home,
opens a library, a title and search (20 route changes in all, SC-004); the
`<audio>` `currentTime` keeps rising, it never pauses, and no second request
for the stream URL or second `play` RPC is made.

### US2 — a queue I can add to from any library page (P1)

Every playable audio item offers **Play**, **Play next** and **Add to queue**:
in the album track list (per row, via a row menu button and right-click),
on album and artist headers (Play, Shuffle, Add to queue: all tracks in disc
and track order), and on song tiles in search and library grids. Playing a
track from an album list makes the queue the album from that track on, so
Next moves through it. A short toast confirms "Added to queue" with Undo.

The now-playing screen gains a queue panel: the current entry, up next
(reorder by drag and by keyboard Alt+↑/↓, remove, jump to), history
(collapsed), Clear, Shuffle and Repeat (off, all, one). When the current
entry ends, the next one starts; at the end of the queue playback stops on
the last entry, unless Repeat all. Previous restarts the entry when more than
3 s in, else goes to the one before. Media Session next/previous (013 FR-009)
and the 011 player keys drive the same queue.

Queue rows show title and artist. `ItemSummary` has no artist or album today,
so PR 2 appends `artist @12 :Text` and `album @13 :Text` to `ItemSummary`
(append-only, schema version bump), filled for `audio` from the stored album
artist and album.

**Independent test**: unit tests for every queue operation (pure Rust, no
browser); Playwright adds a second album's track with Play next while the
first plays, lets the first end (short fixture or seek to its last second)
and sees the added track play next, then reloads and finds the queue restored
and paused at the saved position.

### US3 — a video shrinks into a mini window when I leave it (P2)

Leaving the watch page while a video plays (Back, the rail, a link, Escape)
shrinks the same video into a floating 16:9 window in a bottom corner (about
320 px wide on desktop; above the now-playing bar on narrow screens) with
play/pause, expand (back to the watch page at the same position) and close.
It can be dragged to another corner. The full and mini layouts are one
element with two classes, so the picture and buffer carry over without a
remount or renegotiation. A Picture-in-picture button (013 US2) on both the
watch page and the mini window hands the video to the browser's own PiP
window for use outside the tab.

Video entries can be queued like songs (Play next, Add to queue on movies and
episodes); an episode still offers its B292 previous/next, which are separate
from the queue.

**Independent test**: Playwright plays a video, navigates home, sees the mini
window's `<video>` with `currentTime` still rising and no new stream request,
presses expand and is back on the watch route at that position.

### US4 — playlists (P3, spec 021, not built here)

Proposed, to be specified separately: a native `Playlists` capability over the
existing `PlaylistStore` through a shared service (V172); a Playlists tab in
music libraries (012 US9); "Add to playlist" next to "Add to queue"; "Save
queue as playlist"; play a playlist as the queue. It needs 017 T061/T062 for
playlist ids in `Library.query`/`detail` first.

## Requirements

- **FR-001** One playback per app. The media element, the engine, the report
  clock and the queue live in a `NowPlaying` context provided by `NativeApp`
  and drawn by a `Stage` component that is a sibling of `Screen { route }`
  inside `Shell`, never inside `Screen` or a route's `remount_on` list.
  Routes only read the context and send it commands.
- **FR-002** The watch route and the now-playing screen are views of the
  stage: on them the stage is in its full layout (V235's fixed full-viewport
  player for video, `ListenView` for audio); on any other route it is in its
  mini layout. The layout is a class on the same node. Changing it never
  re-attaches the engine.
- **FR-003** A new play session still mounts fresh engine and report state
  (V236): the stage keys an inner `Session` component on the play session id
  with `remount_on`, while the element that owns the stream stays put.
- **FR-004** Reporting is per entry: start when an entry starts, progress on
  the 013 FR-006 cadence, Stopped when the entry ends, is skipped, the stage
  is closed, the user signs out or `pagehide` fires. A layout change reports
  nothing. Hiding the tab does not stop audio (013 edge cases).
- **FR-005** Queue model in `native/queue.rs`, pure and unit tested: entries
  carry a client entry id, item ref, kind, title, artist, image and runtime;
  operations play now (replace with a context and a start index), play next,
  add, remove, move, jump, clear, shuffle (a permutation that keeps the
  current entry first and is undone by turning shuffle off) and repeat. Audio
  and video share one queue (013 FR-007).
- **FR-006** The queue and the current entry's position persist in
  `localStorage` (`pharos.queue.v1`, per signed-in user id) on every change
  and every progress tick. After a reload the stage restores paused at the
  saved position and does not autoplay. An entry whose item no longer
  resolves is dropped with a console warning.
- **FR-007** Reconnecting (machine `reconnecting`) keeps the element and the
  queue; an entry whose grant has lapsed renegotiates from the last position
  through the same path `WatchScreen` uses today.
- **FR-008** Manage is a different app (`Root` renders `RootApp`), so going
  to Manage stops playback and reports Stopped. Accepted for v1; noted in the
  Manage link's tooltip.
- **FR-009** Instrumentation: every queue operation logs one console info line
  (`pharos: queue <op> entries=<n> current=<i>`) and the RPC calls it causes
  carry `ui_action` (`queue_next`, `queue_add`, `auto_advance`, `restore`),
  so server logs show how a playback started; a failed advance is a
  `playbackFailed` client event with the entry's item id and the queue length.

## Success criteria

- **SC-001** (013 SC-004) A song plays through 20 route changes with no pause
  and one stream request.
- **SC-002** A queued track starts within 1 s of the previous one ending on a
  LAN (measured from `ended` to `playing` in the Playwright case).
- **SC-003** A video carries over to the mini window and back with no new
  stream request and less than 0.5 s position drift.
- **SC-004** After a reload, the queue, current entry and position are back.

## Edge cases

- The current entry is removed from the queue: playback moves to the next
  entry, or stops if none.
- The same item queued twice: entries are distinct by entry id.
- Play now on a video while music plays: the queue is replaced, as on any
  Play; Play next and Add to queue never interrupt.
- An HLS video entry in the mini window keeps hls.js attached; a browser that
  cannot play HLS falls back to the existing "needs HLS" message on expand.
- SyncPlay (014): while in a group, the group's queue is the queue and the
  local queue is set aside, restored on leaving. Out of scope until 014 lands
  its native queue.
- Sign out clears the stage and that user's saved queue is kept for their
  next sign-in on this device.

## Decisions

- **Open, for Alison**: what leaving a playing video does. Recommended: the
  mini window (US3). Alternatives: stop as today, or hand it straight to the
  browser's PiP window.
- **Assumed**: the v1 queue is per device in `localStorage`; a server-side
  `Queue` capability (resume the queue on another device, remote control,
  014 group handover) is a later spec. This amends 013 FR-007.
- **Assumed**: Manage stops playback (FR-008).
- **Assumed**: gapless playback, lyrics and the waveform scrubber stay 013
  US7 follow-ups; v1 only negotiates the next entry about 10 s before the
  current one ends so the gap is the element's source swap.

## Invariants

Ids are assigned when each guard lands (next free after V253).

- The stage element is never a descendant of `Screen` (guarded by a Dioxus
  test that renders two routes and asserts the same element id, and by SC-001).
- A layout change performs no engine attach and no report (FR-002, FR-004).
- Queue operations preserve the entry multiset except add/remove/clear, and
  the current entry is always an index into the entries or none (proptest).
