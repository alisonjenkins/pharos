# Spec 037 — Films split across files

- **Status:** Built (this branch).
- **Date:** 2026-11-10T09:00:00Z
- **ADR:** [0107](../../docs/adr/0107-a-split-film-is-one-item-stitched-while-streaming.md)
- **Ids:** B1190–B1191, V1150–V1155.

## Problem

Older rips often hold one film in two or more files (`… cd1.avi`,
`… cd2.avi`). Each file was its own film in the library: the second part was
listed on its own, with no poster and no match, and the first played and
stopped half way.

## Behaviour

### Stacking (scanner)

- A video file's **part marker** is the last `cd N`, `disc N`, `disk N`
  (anywhere in its name) or `part N` / `pt N` (only after a year: before a
  year `Part 1` is part of a title), with or without a separator and a leading
  zero.
- Files in **one folder** whose names are equal once the marker is dropped
  (case and punctuation folded) are **candidates**. Their containers may
  differ.
- Candidates become **one film** (V1150) only when:
  1. the part numbers run exactly 1..=n, n ≥ 2 (`numbering`);
  2. no part has versions of its own (`versions`);
  3. every part has been probed (`unprobed`): the first scan indexes them
     apart, the next decides;
  4. every part's probe found a non-zero duration (`no_duration`), since the
     playlist can only run on past a part of known length;
  5. no two parts' embedded titles, with any part marker dropped, differ
     (`titles_differ`). Files named as parts that hold different films (a
     series ripped as `cd1`…`cd4`) stay apart (V1151).
- The first part is the item; each later part is stored on it as an
  alternate with `part: Some(n)` in `alternate_sources_json` (no schema
  change). A stacked group is never folded as a copy of another file.
- A part's old row hands over its played state and favourite, never its
  resume point, which was a time in that part (V1152).
- Every decision is counted in `pharos_scan_part_stack_total{outcome}`
  (`stacked`, `numbering`, `versions`, `unprobed`, `titles_differ`,
  `no_duration`), and a
  refusal is logged as "left a film's parts apart" with the deciding value and
  the files.

### Listing

- A split film's parts are not versions (V1153): PlaybackInfo's
  `MediaSources`, the native version picker, the duplicates report, the 3D
  badge and `MediaSourceId` version selection read `MediaProbe::versions()`.
- Run time is the sum of the parts' (V1155): `RunTimeTicks`, played-state
  thresholds, SyncPlay and group-seen read `MediaProbe::run_time_ms()`.

### Playback

- A split film never direct plays or direct streams (V1155): negotiation
  runs as for a URL-backed item, with no DirectPlayProfiles, and reports
  `direct_play_block="split_film"` with the part count.
- Every HLS media playlist of a split film (mpegts `hls1`, VP9 fMP4 with its
  audio rendition, h264 fMP4 and its rungs) lists the first part's segments,
  then for each later part `#EXT-X-DISCONTINUITY`, the part's own
  `#EXT-X-MAP` (fMP4) and its segments (V1154). A part's URLs carry
  `MediaSourceId={item}-part-{n}`, so the existing segment, init and audio
  handlers serve it from its own file, grid and cache keys. The scanner
  only stacks parts with a duration; should one still be missing (a row
  edited by hand), the playlist ends before it and logs the part.
- `pharos_media_source_selection_total{requested="part"}` counts part
  segment requests.

## Not covered yet

- Subtitles, chapters, trickplay, intro/credits markers and the cold-start
  prewarm cover the first part only.
- The watcher still indexes a newly written part file alone until the next
  full scan stacks it (as it does for edition files).
