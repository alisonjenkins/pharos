# ADR-0113: Next Up follows the last played episode of each started series

- **Status:** Proposed
- **Date:** 2026-11-10T12:30:00Z
- **Deciders:** Alison Jenkins

## Context

Next Up listed, for every series the viewer could see, the lowest-numbered
episode they had not played, sorted by series name and cut at the rail's
size. So a show nobody had started filled the rail, a show being watched
late in the alphabet never appeared, a season 0 special won as "lowest",
and an episode in progress showed in both Continue Watching and Next Up.
Building it bound every visible item's id into `user_data_bulk`, so its cost
grew with the library rather than with what the viewer had watched
(spec 038 G1, FR-101 to FR-105).

## Decision

We will build Next Up from the viewer's own play state:

- A series is *started* when the viewer has played, or has a resume point
  on, at least one of its episodes. Only started series are listed.
- The *anchor* is the episode played or started most recently
  (`last_played_at`), the later in airing order (season, episode) on a tie.
- The pick is the first unplayed episode after the anchor in airing order.
  A season 0 special is a candidate only when the anchor is itself a
  special. A started series with no such episode is left out.
- An episode in progress belongs to Continue Watching: when the anchor or
  the pick has a resume point, the series is left out of the home rail.
- Series are ordered by the anchor's `last_played_at`, newest first, then
  by name.
- The started rows come from `UserDataStore::started_items(user, Episode)`,
  one query over the viewer's own rows joined to `media_items.kind`. The
  series' episodes come from the cached item list, and one `user_data_bulk`
  bound to the picks alone gives each pick its own row (favourite, chosen
  tracks).
- A Series page request (`SeriesId`) uses the same anchor rule, but lists an
  in-progress anchor itself (there is no Continue Watching beside it) and,
  for a series not started, its first unplayed regular episode.

`pharos_next_up_total{outcome}` counts each listing as `listed`, `empty` or
`error`, and a debug line carries how many series were considered and kept.

## Consequences

- A new viewer sees an empty Next Up until they play something, which is
  what Jellyfin does too.
- Rewatching an early episode moves Next Up back to the one after it.
- Marking a whole series played at once, with equal `last_played_at`,
  anchors on its last episode, so the series drops out once complete.
- The cost of Next Up follows the viewer's history. A viewer who has
  watched thousands of episodes reads that many rows, still far fewer than
  every item in the library.

## Alternatives considered

- *Keep the lowest unplayed episode, only for started series.* Fixes the
  unstarted shows but still offers episode 1 to someone who skipped it, and
  ignores where they actually are.
- *Anchor on the highest played episode in airing order.* Simpler, but a
  rewatch of an earlier season would never be followed.
- *Show the next episode after an in-progress one.* Skips the episode the
  viewer is part way through; Continue Watching already offers it.

## References

- `specs/038-ux-round-2/spec.md` G1, FR-101 to FR-105.
- `crates/pharos-server/src/services/library.rs` `next_up`, `pick_next`.
- `crates/pharos-server/tests/it_rpc/library.rs`
  `next_up_follows_the_last_played_episode`.
