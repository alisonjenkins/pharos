# Feature specification: playback that follows you around pharos-ui

**Feature directory**: `specs/020-ui-now-playing/`
**Created**: 2026-11-03T22:10:00Z
**Revised**: 2026-11-04T14:30:00Z (the play queue landed as B435–B437; Alison
chose the floating window, the mini-player bar, the swap button and a
confirmed Clear queue)
**Status**: ready
**Input**: "The player should enable us to explore the library while playback
continues, for example we might want to go explore the music library while
listening to music still to queue more tracks into a queue or maybe make a
playlist." (Alison, 2026-11-03). "I vote we have a player that persists for
the whole session and appears as a small floating window in the corner that
can be moved to an arbitrary place on the page with the option to make it a
mini player. As you say the watch and now playing screens show the full size,
we should be able to swap between the views by pressing a button. [...] we
should also have a clear queue button (with confirmation)." (Alison,
2026-11-04)
**Depends on**: 009 FR-008 (shell, mini-player slot), 013 US2/US7, 013
FR-007, 013 FR-009, 013 SC-004, the music play queue (B435–B437, V375–V377, `native/queue.rs`),
V235 (full-viewport watch page), V236 (a new play session mounts a new player),
V315 (full-window now-playing screen)
**Delivers**: 013 US7's persistent mini-player, 013 SC-004
**Ids**: B460–B463, V400–V403 (reserved 2026-11-04). ADR: one, numbered when
PR 1 merges.
**Follow-up (not built here)**: playlists in pharos-ui (012 US9) as their own
spec

## Problem

Playback lives inside the watch route. `Screen { route }` renders
`WatchScreen`, which owns `Player`: the engine, the report clock and the
`<audio>`/`<video>` element. Any navigation swaps `WatchScreen` out, `use_drop`
detaches the engine and reports Stopped. The play queue (B435) already lives in
the app's context and survives navigation, but the music it queues does not:
leaving the now-playing screen to find the next album ends the song.

## What already exists

- **Play queue** (B435–B437): one app-wide `Queue` (`QueueCtx`) with Play next,
  Add to queue, drag, remove, shuffle and Clear; a song that ends plays the
  queue's first, else its album's next. Clear acts at once, with no
  confirmation.
- **Playlists** (T70): fully stored server-side (`PlaylistStore`, Jellyfin
  `/Playlists/*`), not yet in the native API (017 T061/T062).
- **Server play queue**: none for a single user. Jellyfin clients send
  `NowPlayingQueue` on progress, which pharos ignores; the only server queue is
  the SyncPlay group's (014). The client queue stays client-side.

## User stories

### US1 — playback lasts the whole session (P1)

Start a song or a video, then go anywhere in the app except Manage: it keeps
playing. The player has three views:

- **Full**: the watch page (V235) or the now-playing screen (V315), as today.
- **Floating**: a small window, 16:9 for video and a cover card for music
  (about 320 px wide on a desktop, 60 % of the width on a phone), with the
  title, play/pause, previous/next, a thin progress line and buttons for
  Full, Mini and Close. It starts in the bottom-right corner and can be
  dragged by its title strip to any place on the page; it stays inside the
  window when the window is resized. The spot is remembered for the tab.
- **Mini**: a bar along the bottom of the shell with cover, title, the same
  transport, a progress line and buttons for Full, Floating and Close. The
  page reserves its height so it hides nothing.

Leaving the full view by navigating (the rail, a link, Back) puts the player in
the view last used outside full, floating the first time. Starting something
new from a library page (Play on a track, an episode or a film) while the
player is floating or in the bar replaces what plays and keeps that view; it
opens the full view only when no player was showing. The full view gains a
button that swaps to that view without navigating; Full on the floating window
or the bar returns to the watch or now-playing route for the item. Close stops
playback, reports Stopped and removes the player. Opening and leaving the full
view never restarts the stream.

**Independent test**: Playwright plays the seeded "carefree" song, then goes
home, opens a library, a title and search (20 route changes in all, 013 SC-004);
the `<audio>` `currentTime` keeps rising, it never pauses, and no second
request for the stream or second `play` RPC is made. Then it drags the
floating window to the top-left, swaps it to the bar, and back to full.

### US2 — the queue keeps playing in the background (P1)

When a song ends while the player is floating or in the bar, the queue's next
song plays there without opening the full view. Previous and next on the
floating window and the bar step through the queue as they do on the
now-playing screen.

**Independent test**: Playwright queues a second song with Play next, floats
the player, seeks to the last second and sees the second song play in the
floating window while the route stays where it was.

### US3 — clearing the queue asks first (P1)

The queue drawer's Clear asks "Clear the queue? This removes N songs." with
Clear and Cancel buttons drawn in place (not a browser dialog). Escape or
Cancel leaves the queue alone; Clear empties it. An empty queue offers no
Clear.

**Independent test**: Playwright queues an album, presses Clear, cancels and
sees the queue unchanged, then presses Clear and confirms and sees it empty.

### US4 — picture-in-picture (P3)

A Picture-in-picture button (013 US2) on the watch page and the floating
window hands the video to the browser's own PiP window, for watching outside
the tab. Hidden where the browser has no PiP.

**Independent test**: Playwright on Chromium presses the button and sees
`document.pictureInPictureElement` set to the stage's `<video>`; with
`document.pictureInPictureEnabled` stubbed false the button is absent.

## Requirements

- **FR-001** One playback per app. `NativeApp` provides a `NowPlaying` context
  (the item playing, the view, the floating spot). A `Stage` component, a
  sibling of `Screen { route }` inside `Shell` and never inside `Screen` or a
  route's `remount_on` list, renders `WatchScreen` for that item. Play
  actions (library Play, a queue pick, Watch together, a group's next item)
  go through `NowPlaying::play(item)`, which keeps the current view when a
  player is showing and opens the full view otherwise. Only the watch route
  reached directly (a typed URL, reload, Full) sets the full view; the route
  renders nothing itself.
- **FR-002** The view is a class on the stage's wrapper. Changing it never
  remounts `WatchScreen` or `Player`, so it never re-attaches the engine or
  sends a report.
- **FR-003** A new item or play session still mounts a fresh `Player` (V236):
  the stage keys `WatchScreen` on the item and `Player` on the play session,
  as today.
- **FR-004** Moving to another item (next, previous, queue pick, a song
  ending) navigates with `replace` in the full view, as today, and outside it
  changes the stage's item without touching the route.
- **FR-005** Reporting stays per playback: Stopped when the item changes, on
  Close, on sign-out and on `pagehide`. Hiding the tab does not stop audio.
- **FR-006** The floating spot is a pair of fractions of the viewport (so a
  resize keeps the window on screen), clamped so the whole window stays in
  view, and is kept in `sessionStorage` (`pharos.stage.spot`). The last
  non-full view is kept there too (`pharos.stage.view`).
- **FR-007** Dragging uses pointer events with pointer capture on the title
  strip, which has `touch-action: none` so a finger drag moves the window
  rather than scrolling the page; it works with a mouse, a pen and touch, and
  the strip's buttons are not drag handles. The window and the bar are keyboard reachable; the
  floating window moves by 16 px per arrow key while its strip has focus.
- **FR-008** Outside the full view the stage draws none of the full view's
  chrome and stops the visualiser's GPU work; the media element stays in
  place.
- **FR-009** Manage is another app (`Root` renders `RootApp`), so going to
  Manage stops playback. Accepted for v1.
- **FR-010** Instrumentation. Server side, the `play` RPC a stage makes
  outside the full view carries `ui_action=stage_<floating|mini>` (in place of
  `play`), so the server's call logs count background starts:
  `sum by (ui_action) (count_over_time({namespace="pharos"} |= "ui_action" |= "stage_" [1d]))`
  (the exact line is pinned when PR 1 lands). Client side, one console line
  per view change
  (`pharos: stage view=<full|floating|mini> item=<id> reason=<nav|button|close>`)
  and per drag end (`pharos: stage spot x=<f> y=<f>`); a confirmed clear logs
  `pharos: queue action=clear upcoming=0` (existing) and a cancel
  `pharos: queue clear cancelled upcoming=<n>`. View changes and drags make
  no server call, so they have no server signal.

## Success criteria

- **SC-001** (013 SC-004) A song plays through 20 route changes with no pause
  and one stream request.
- **SC-002** A video swapped to floating and back to full makes no new stream
  request and drifts less than 0.5 s.
- **SC-003** A dragged floating window stays wholly in view after the window
  shrinks to 400 × 600.

## Edge cases

- Nothing follows when a song ends in the background (the queue is empty and
  it is the album's last, B435's `Queue::advance`): playback stops on that
  song and the player stays, paused at the end.
- A SyncPlay group drives the player in any view; the group decides the next
  item, as today.
- Signing out removes the player (the session ends).
- A failed playback in the floating window or the bar shows its error there,
  with Full and Close.

## Invariants

Reserved: V400–V403.

- The stage is never a descendant of `Screen`: a Dioxus test renders the watch
  route, then another route, and counts one `Player` mount.
- A view change performs no engine attach and no report.
- The floating window's top-left is clamped so as much of it as fits is in
  view: wholly inside the viewport when it fits, else pinned to the top-left
  corner (unit tests over the clamp, and SC-003).
