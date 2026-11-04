# ADR-0049: One player for the session, on a stage beside the routed screen

- **Status:** Accepted
- **Date:** 2026-11-04T16:00:00Z
- **Deciders:** Alison (chose the floating window, the mini bar and the swap button), Claude

## Context

The native UI mounted its player inside the watch route: `Screen { route }`
rendered `WatchScreen`, which owned the engine, the report clock and the
`<audio>`/`<video>` element. Any navigation unmounted it, detached the
stream and reported Stopped, so a listener could not browse for the next
song while one played (spec 020). Moving a media element to another parent
detaches an MSE source (hls.js), so a player that outlives the route cannot
be re-parented per page either.

## Decision

We will mount the one player on a stage that is a sibling of the routed
screen inside the shell (`StageView` in `native/app.rs`, state in
`native/stage.rs`). The page root holds a `Stage` (the item playing, its
view, the floating window's spot) in a `StageCtx`. The watch route only
sets the stage's item and full view and draws nothing itself; any other
route shrinks the player to its compact view (floating or mini, the last
one used). The view is a class on the stage; it never remounts the player.
While the player is compact, a navigation to a watch route is a play the
stage takes in place: the page stays and the stage plays the item. The
floating window's spot is a fraction of the room around it, so it is in
view at any viewport size.

## Consequences

- Leaving the watch page no longer stops playback; Back and the new Shrink
  button shrink the player, Close stops it.
- Every play path (library Play, queue, Watch together, a group's next
  item) goes through `Nav`/`Replace`, which consult the stage; a new path
  that sets the route directly would bypass it.
- A play RPC made while compact carries `ui_action=stage_floating|stage_mini`.
- `/manage` is another app and still stops playback.
- Picture-in-picture (spec 020 US4) and a server-side queue are separate
  changes.
