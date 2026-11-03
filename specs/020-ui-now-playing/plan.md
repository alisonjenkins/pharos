# Implementation plan: playback that follows you around pharos-ui

**Spec**: [spec.md](spec.md)
**Revised**: 2026-11-04T14:30:00Z

## Shape

```text
Root
└─ NativeApp            provides Ctx, QueueCtx and NowPlaying (item, view, spot)
   └─ Shell
      ├─ Screen { route }   Route::Watch(id) only sets NowPlaying; others as today
      └─ Stage              .pharos-stage.is-full|is-floating|is-mini
         └─ remount_on(item) → WatchScreen → remount_on(session) → Player
```

`WatchScreen` and `Player` move from `Screen` to `Stage` unchanged in what they
negotiate and report. `Player`'s `go` (next, previous, queue pick, a song that
ends) asks `NowPlaying` for the item, which replaces the route only in the full
view. `ListenView` and `WatchView` take the view and, outside full, draw the
compact chrome (`StageBar`) over the same media element.

## Pull requests

Each is one PR of atomic commits, reviewed with /review, then queued.

1. **Persistent player (US1, US2)**: `native/stage.rs` (`NowPlaying`, views,
   spot maths with unit tests for clamping), `Stage` in the shell, the compact
   chrome, the drag (pointer capture through `native/web.rs`), the swap
   button in both full views, CSS (tokens only). Ships the ADR for the stage
   boundary (its own commit, a row in `docs/adr/README.md`, number taken when
   the merge queue calls the PR). Guards: the US1 and US2 Playwright cases;
   the one-`Player`-mount Dioxus test; the clamp unit tests.
2. **Clear queue confirmation (US3)**: in-place confirm in the queue drawer.
   Guard: the US3 Playwright case.
3. **Picture-in-picture (US4)**. Guard: the US4 Playwright case.

## Risks

- hls.js and layout: moving the `<video>` between parents detaches the MSE
  source, so the stage never changes parent (FR-002); the view is CSS only.
- `.pharos-watch` and `.pharos-listen` are `position: fixed; inset: 0`
  (V235, V315); the floating and mini rules override that under
  `.pharos-stage:not(.is-full)`.
- Event delegation (pharos-ui gotcha): handlers resolve their element with
  `target().closest(..)`.
