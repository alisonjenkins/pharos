# Implementation plan: check that subtitles fit their video (040)

**Spec**: [spec.md](spec.md). **ADR**: ADR-0122.

## Shape

No new service and no change to the aligner's HTTP contract. The work is in the server, beside the styling job.

| Piece | Where | Holds |
|---|---|---|
| Probe picker | `pharos-subs` (`check::probes`) | FR-004/005 cleaning and spreading, pure, property-tested |
| Language gate | `pharos-subs` (`check::lang`) | FR-002/003: ISO 639 normalising, Latin share, stopword test |
| Verdict | `pharos-subs` (`check::verdict`) | FR-007/010 rules over score vectors, pure; thresholds passed in |
| Coarse search | `pharos-server` (`subtitle_check::coarse`) | FR-009: one 8 kHz decode through the libav worker pool, speech bitmap, correlation per ratio |
| Job | `pharos-server` (`subtitle_check`) | FR-015 to FR-018: passes, rows, on-demand FIFO, karaoke gate, aligner client reuse from `services::lyrics::align` |
| Store | `pharos-store-sqlx` | one table `subtitle_check` (item id, source kind + index or sidecar path hash, mtimes/sizes, `CHECK_VERSION`, verdict, reason, offset_ms, ratio, scores JSON) and one migration; number from the merge queue |
| Serving | `api/jellyfin/subtitles.rs` | FR-012: the cue transform in `deliver_srt`, `deliver_vtt`, `deliver_ass_inner`, after extraction and before conversion; `?raw=1` |
| Track list | playback info / track DTOs | FR-013 suffixes and default-selection rule |
| Admin list | Manage page | FR-011 / story 3: items with `wrong_video` or `segmented`, with reasons |

## Karaoke gate

The align queue in `services::lyrics::align` exposes whether a job is queued or running. The check job asks before each request and waits (polling every few seconds, as the styling job does for the transcode scheduler). The aligner itself already serves one request at a time (039 R5), so a check request in flight delays a new karaoke song by at most one request, about 4 s on the GPU.

## Cost

| | Per track | Basis |
|---|---|---|
| Aligner, stage 1 (fits) | about 8 s GPU | 2 requests of about 4 s (spike) |
| Aligner, stage 2 worst case | about 32 s GPU | 8 requests |
| Aligner on CPU fallback | about 6x the GPU time | 039 benchmarks |
| Coarse search | a few seconds of one CPU core, only on stage 2 | 8 kHz decode + bitmap |
| Server RAM | under 1 MB per check | scores, bitmap |
| Aligner VRAM | none extra | freed after each request (#1294) |

## Slices

0. `pharos_lyrics_align_wait_seconds` in the align queue, on its own bucket ladder (1 ms to 1 h, so a wait past 30 s is not capped and a rise within the first 100 ms still shows), with a test that fails without it (FR-022).
1. Probe picker, language gate and verdict as pure code in `pharos-subs`, with the spike's numbers as test vectors (generic names).
2. Table, migration and the job with `apply = false`: checks run and log, nothing is served differently. Calibration (FR-014) reads these logs.
3. Coarse search and stage 2.
4. Sibling check (FR-011) and the admin list.
5. Serving correction and track-list marks, behind `apply`.
6. Calibration run, then `apply = true` by config.

## Signals to name before building (ODD)

- Verdict mix: `sum by (verdict, reason_class) (increase(pharos_subtitle_check_total[1d]))`.
- Karaoke contention: `histogram_quantile(0.95, sum by (le) (rate(pharos_lyrics_align_wait_seconds_bucket[1h])))` while the backfill runs, against the week before. This metric is new (FR-022) and is slice 0, shipped before anything else. It only covers the server queue, so the comparison also reads the wait inside the aligner: `quantile_over_time(0.95, {namespace="pharos"} |= "lyrics: aligned" | json | unwrap fields_first_line_ms [1d]) by ()` (one p95 over all jobs, not one per extracted label set) (the server logs JSON, fields under `fields`). Both must hold for SC-003.
- A track's reasoning: `{namespace="pharos"} |= "subtitle check" | json | item_id="<id>"`.
