# ADR-0107: A split film is one item, stitched while streaming

- **Status:** Accepted
- **Date:** 2026-11-10T09:00:00Z
- **Deciders:** Alison (playback and storage choices, 2026-11-10), Claude

## Context

A film held in several files (`cd1`, `cd2`) showed as several films, the later
parts unmatched and without art, and the first part stopped half way. The
owner chose (2026-11-10) that such a film plays as one continuous film with
one seek bar, run time and resume point on every client, and that the parts
are joined while streaming rather than by keeping a joined copy on disk.

Files named as parts are not always one film: a series ripped as `cd1`…`cd4`
holds a different film in each.

## Decision

We will stack a split film's files into one item in the scanner and stitch
its parts in the HLS playlists.

- The first part is the item. Each later part is an `AlternateMediaSource`
  with `part: Some(n)`, stored in the existing `alternate_sources_json`
  column, so no migration is needed. `MediaProbe::versions()` and `parts()`
  tell editions from parts.
- Files are stacked only when their names agree but for the part marker, the
  numbers run 1..=n, no part has versions, every part has been probed, and no
  two embedded titles differ.
- Every HLS media playlist lists the parts in order, each after an
  `#EXT-X-DISCONTINUITY` with its own `#EXT-X-MAP`, and each part's URLs
  select it by `MediaSourceId`, so the segment pipeline serves a part exactly
  as it serves an edition.
- A split film never direct plays (`direct_play_block="split_film"`).

## Consequences

- Seeking, resume and run time span the whole film on every HLS client with
  no extra disk and no change to the segment, cache or transcode code.
- A split film always streams through HLS, so it always costs a transcode.
- The player must honour discontinuities: hls.js, AVPlayer and ExoPlayer do.
- Subtitles, chapters, trickplay, markers and the cold-start prewarm cover the
  first part only until follow-up work.
- A newly added part shows alone until the scan after it is probed.

## Alternatives considered

- **Keep a joined copy** of each split film in the cache and play that file:
  everything, direct play included, works unchanged, at about 5–25 GB of
  disk. Declined by the owner in favour of no extra disk.
- **Queue the parts as separate files** (Jellyfin's `AdditionalParts`): the
  seek bar and resume point cover one part, and many clients do not advance.
  Declined by the owner.
- **ffmpeg's concat demuxer** as one virtual input: needs identical stream
  layouts across parts, and every probe, image and subtitle path takes a
  file path.

## References

- [Spec 037](../../specs/037-multipart-films/spec.md)
- V1150–V1155 in `specs/001-pharos-baseline/invariants.md`
