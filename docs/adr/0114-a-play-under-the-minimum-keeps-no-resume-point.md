# ADR-0114: A play under the minimum resume point keeps no resume point

- **Status:** Proposed
- **Date:** 2026-11-10T19:30:00Z
- **Deciders:** Alison Jenkins

## Context

Every progress report and every stop stored its position. A film opened by
mistake, a trailer peek, or a song skipped after ten seconds therefore sat
in Continue Watching until it was played to the end or marked played
(spec 038 G1, FR-107). Jellyfin has the same problem and solves it with a
minimum resume percentage.

## Decision

We will not store a resume position that is under the minimum resume point:

- Video (films and episodes): the larger of `[server].min_resume_pct` of the
  runtime (default 2) and `[server].min_resume_secs` (default 60). With an
  unknown runtime, the seconds alone.
- Songs: `min_resume_secs`.
- Books have no minimum (they keep a reading position, not a play position).
- The rule applies to the position being stored, not to how long this
  session played. A stop or report under the minimum writes nothing, so a
  resume point or played mark stored earlier stands.
- It applies to the stop and to progress reports, so a player that never
  sends its stop leaves nothing behind either. The live session and the
  socket fan-out still see every report.
- The settings sit beside `played_threshold_pct` and are clamped
  (at most 10% and 600 s). `AppState::new` starts with the rule off; boot
  sets it from the config.

`pharos_play_stop_total{outcome="below_minimum",reason="video"|"song"}`
counts a skipped stop, and
`pharos_progress_writes_total{outcome="skipped",reason="below_minimum"}` a
skipped report.

## Consequences

- Watching the first minute of a film and stopping leaves no trace in
  Continue Watching. The item's `last_played_at` does not move either.
- Rewinding to the start and stopping keeps the old resume point rather
  than clearing it. Mark unplayed clears it.
- Positions stored before this change are not cleaned up; they leave
  Continue Watching as they are played or marked.

## Alternatives considered

- *Filter Continue Watching at read time.* It would also hide old rows, but
  it needs every resumable item's runtime on each listing, and the stored
  row would still say the item was started (Next Up reads it).
- *Clear the resume point on a stop under the minimum.* Loses a real resume
  point when someone rewinds by mistake, which FR-107 rules out.
- *A percentage alone.* Two percent of a 20-minute episode is 24 s, which
  keeps the wrong-click case; the seconds floor covers short items.

## References

- `specs/038-ux-round-2/spec.md` G1, FR-107.
- `crates/pharos-server/src/services/play_state.rs` `MinResume`, `stop`,
  `report`.
- `crates/pharos-server/tests/it_playback/stop_outcomes.rs`
  `a_stop_under_the_minimum_keeps_no_resume_point`,
  `a_progress_report_under_the_minimum_is_not_written`.
