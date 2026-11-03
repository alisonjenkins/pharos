# Implementation plan: playback that follows you around pharos-ui

**Spec**: [spec.md](spec.md)
**Created**: 2026-11-03T22:10:00Z
**Builds on**: #590 (now-playing controls) once merged; rebases over
`feat/ui-player-mouse` (wheel and Shift+click shortcuts on the player), which
moves with the player code in PR 1.

## Shape

```text
Root
└─ NativeApp            provides Ctx and NowPlaying (queue, current entry, layout)
   └─ Shell
      ├─ Screen { route }   routes read NowPlaying, send it commands
      └─ Stage              the one <audio>/<video>, mini or full by class
         └─ remount_on(play session) → Session   engine, report clock, reporting
```

`WatchScreen` stops owning playback: it tells `NowPlaying` to play its item
(or, when that item is already current, just switches the stage to full) and
renders nothing of its own but the route. `Player`'s negotiation, engine
attach, track renegotiation and reporting move into `Session` unchanged, so
V236's `a_new_play_session_mounts_a_new_player` keeps its meaning against
`Session`.

## Pull requests

Each is one PR of atomic commits, reviewed with /review before Alison is asked
to merge.

1. **Persistent stage and the now-playing bar (US1)**, about 1 day.
   `native/now_playing.rs` (context, commands, layout), `Stage` and
   `Session` split out of `WatchScreen`/`Player`, the bar component and its
   CSS (tokens only), the `/ui/listen` route for the full audio view.
   Queue of one. Guards: the SC-001 Playwright case; a Dioxus test that the
   stage element survives a route change; V236's test moved to `Session`.
2. **Queue (US2)**, about 1 to 1.5 days. `native/queue.rs` with unit and
   property tests; `localStorage` persistence; auto-advance and next-entry
   pre-negotiation; Play next / Add to queue on track rows, headers and song
   tiles; queue panel in `ListenView`; Media Session next/previous; the
   `artist`/`album` fields on `ItemSummary` (schema bump, agreement test with
   the Jellyfin DTO's `AlbumArtist`/`Album`). Guards: the US2 Playwright case.
3. **Video mini window and PiP (US3)**, about 1 day. Mini layout for video,
   corner drag, expand/close, PiP button, queueing movies and episodes.
   Guard: the US3 Playwright case.
4. **Playlists (spec 021)**, specified after 3 lands.

## Risks

- hls.js and layout: moving the `<video>` between parents would detach the
  MSE source, so the stage never changes parent (FR-002). A CSS-only layout
  change is safe for MSE.
- Autoplay policy: auto-advance happens inside an already-playing element, so
  browsers allow it; restore after reload deliberately does not autoplay.
- Event delegation (pharos-ui gotcha): the bar's buttons resolve their
  element with `target().closest(..)`, as the rest of the shell does.
