# Feature specification: check that subtitles fit their video, and fix their timing (040)

**Created**: 2026-11-12T19:15:00Z
**Status**: Draft, for review. Nothing built.
**Scope**: A background job that checks each text subtitle track against its video's audio with the aligner (spec 039, ADR-0116). It says whether the track belongs to the video, measures a constant offset or a speed ratio, and Pharos applies that correction when it serves the track. The media files are never written.
**Decision record**: ADR-0122. **Bugs reserved**: B1376-B1385. **Verification**: V1346-V1355.

---

## Problem statement

- **Subtitles from another video go unnoticed.** On 2026-11-11 two episodes of one series had their video files swapped on disk. Each file's name-matched subtitles were right for the name and wrong for the picture, and nothing in Pharos could tell. It was found by hand after a viewer noticed.
- **Copies retimed by other tools can be junk.** The same swap fed an outside sync tool the wrong audio, and its `.synced.srt` copies were wrong too. Pharos serves them as readily as good ones.
- **Mistimed subtitles must be fixed by each viewer.** A track early or late by a few seconds, or drifting because it was timed for a 25 fps release, can only be corrected today with the player's subtitle delay, per viewer and per playback, and a drift cannot be corrected at all.

## What the spike showed (2026-11-12, read-only, live aligner)

Twelve dialogue cues per track, each sent with its own times padded by ±2 s, against a decoy (the text of a cue at least 10 min away) in the same window. Four tracks with English audio and English subtitles, and one with Japanese audio and English subtitles.

| Case | Median line score | Range |
|---|---|---|
| Right text, per track | 0.95, 0.95, 0.92, 0.67 | pooled min 0.27 |
| Decoy text | 0.25 pooled | max 0.53 |
| Text from the swapped episode | 0.27 pooled | max 0.37 |
| Japanese audio, English text: right / decoy | 0.175 / 0.154 | no signal |

- A per-track rule separated every track: right tracks had 8-12 of 12 cues at 0.5 or more, decoy and wrong-video runs 0-2. A single cue does not: a right cue scored 0.27 where a decoy scored 0.34.
- Offsets of +3 s and -6 s were found to within 35 ms (5-7 of 7 probes agreeing). A +20 s offset and a 25/23.976 speed ratio were not found, and those tracks then scored at wrong-video levels.
- About 4 s of aligner time per 12-cue request on the GPU, warm.
- The sample is small (4 English tracks, 120 English cues). Thresholds below are starting values to be calibrated (FR-014).

## User stories

1. As a viewer, a subtitle track that is early, late or drifting plays in time without me touching the subtitle delay.
2. As a viewer, a track that does not belong to the video I am watching is marked as such in the track menu, so I pick another one instead of reading the wrong story.
3. As the admin, I can list the items whose subtitles do not fit their video, with the reason, and see when the evidence points at a video file swapped with a sibling episode.
4. As the admin, the check never slows karaoke alignment or playback, and costs no extra RAM worth measuring.
5. As the operator, I can see from one query how many tracks each verdict holds and why, and from one log line why a given track got its verdict.

## Requirements

### Scope of a check

- FR-001. The job checks every text subtitle track of a video item: sidecar files, embedded text streams and downloaded subtitles. Image subtitles (PGS, VobSub) are out of scope.
- FR-002. A track is checked with the aligner only when its language equals the language of the audio stream it would play over (the default audio stream, by ISO 639 code after normalising 639-1/-2 forms) and its text is in the Latin alphabet after accent folding (at least 80% of its letters fold into `a`-`z`). Any other track gets verdict `unknown`, reason `language_mismatch:<sub>/<audio>` or `script:<sub>`, and no correction.
- FR-003. A track with no language tag takes the audio's language only when the audio is tagged and the track's text passes a cheap stopword test for that language; otherwise `unknown`, reason `language_untagged`.

### Probes

- FR-004. Up to 12 probe cues are picked, spread evenly over the track's span, each with at least 3 alignable words and text unique within the track. Removed first: SDH tags (`[...]`, `(...)` alone on a line), leading speaker labels (`NAME:`), lines with music notes, ASS lines whose style or override tags mark them as signs or songs (positioned, `\pos`/`\move`, or a style named as a sign/song), and cues shorter than 600 ms. A track with fewer than 6 probes left gets `unknown`, reason `too_few_lines:<n>`.
- FR-005. Each probe has a decoy: the text of another probe-eligible cue at least 10 min away (or the farthest one, in a short file), aligned in the probe's window.
- FR-006. The probes of one candidate go to the aligner as one request and their decoys as a second, using the existing `POST /align` contract unchanged (decoys apart, so they do not sway the offset estimate or the probes' lead clamp). A fitting track costs 2 requests; one that reaches stage 2 at most 2 per candidate tried, capped at 8 per track.

### Verdict

- FR-007. Stage 1 sends the probes at their own times, padded by 2 s each side, using the aligner's existing offset estimate (±8 s). A track **fits** when the median probe score is at least `T_median` (start 0.45) **or** at least half the probes score `T_line` (start 0.5) or more, **and** the median probe score exceeds the median decoy score by at least `T_margin` (start 0.2).
- FR-008. A track that does not fit in stage 1 goes to stage 2, the wide search, before it may be called wrong:
  - Offsets: a coarse search over ±120 s from a speech-activity match (FR-009), then the probes re-sent at the best candidate.
  - Speed ratios: 25/23.976, 23.976/25, 24/23.976 and 23.976/24, each with its own best offset.
  - The first candidate that fits by the FR-007 rule wins.
- FR-009. The coarse search does not use the aligner. It decodes the audio once at 8 kHz mono on the CPU in the background-IO pool, marks 100 ms frames as speech by energy and zero-crossing rate, and slides the track's cue on/off pattern against it (per ratio), keeping the 3 best offsets by correlation. It holds at most the speech bitmap (about 30 KB for 45 min), never the audio.
- FR-010. Verdicts, with their reason carrying the numbers:
  - `fits`: stage 1 fits with an aligner offset under 400 ms (the aligner's negligible shift). Reason `median=<s> decoy=<d> hits=<k>/<n>`.
  - `retimed`: a stage-1 offset of 400 ms or more, or any stage-2 candidate, fits. Reason adds `offset_ms=<o> ratio=<r>`.
  - `wrong_video`: neither stage fits, and the stage-2 best candidate's median is within `T_margin` of its decoys. Reason as `fits`, from the best candidate.
  - `segmented`: some probes fit at one offset and others at another (two or more probes each, offsets more than 2 s apart). Reported only; no correction in this version. Reason lists the offsets.
  - `unknown`: FR-002, FR-003, FR-004, an aligner error or timeout that has used up its retries (FR-015a), or a stage-2 best that is neither fitting nor decoy-level.
- FR-011. A `wrong_video` track of an episode is checked once more against the audio of the previous and next episodes of the same season (stage 1 only). When it fits one of them, the reason adds `fits_sibling=<item id>`, and the admin list says the video file looks like that episode's. No file or metadata is changed.
- FR-011a. Flagged tracks (`wrong_video`, `segmented`) are listed on a Manage tab, "Subtitle check", with the item, track, verdict, reason and a link to the item; an entry leaves the list when a re-check after a file change no longer flags it. Each finished backfill pass that flagged anything adds one Activity entry with the counts (for example "Subtitle check: 3 possible swapped files, 5 tracks that don't fit"), never one entry per track (Alison, 2026-11-12T19:19Z).

### Serving the correction

- FR-012. A `retimed` track is served with the correction applied: each cue's start and end become `start * ratio + offset`, for SRT, WebVTT and ASS output, both for sidecars and embedded streams. The player's own subtitle delay applies on top, unchanged. A query flag `?raw=1` serves the uncorrected track.
- FR-013. The track list names a corrected track with a suffix ("(synced)") and a `wrong_video` track with "(may not match this video)". The track stays selectable. Default track selection skips `wrong_video` tracks when another track of the same language exists.

### Calibration

- FR-014. Thresholds are config values (`[subtitle_check]` `median`, `line`, `margin`) with the starting values above. Every check logs every probe and decoy score, so the thresholds can be recalibrated by query over the library before the verdicts are surfaced in the UI. Surfacing (FR-013 marks and FR-012 serving) is behind `[subtitle_check].apply`, default `false`, until a calibration run over at least 50 tracks with known answers shows no `wrong_video` verdict on a right track.

### Scheduling and cost

- FR-015. The job runs in the mould of the styling job: after a warm-up and the background leader, passes over the library, one track at a time; a row per item and track source keyed by the video's and sidecar's mtime and size and a `CHECK_VERSION`, so each track is checked once and again only when a file or the check changes.
- FR-015a. An aligner error or timeout is not a result about the file. That check stores no verdict row, only an attempt count and a retry time: it is tried again on a later pass at least 1 h later (as the karaoke queue's `RETRY_AFTER`), up to 3 attempts, and only then stored as `unknown`, reason `aligner_error:<last error>`, until a file or `CHECK_VERSION` changes. While the aligner's `/healthz` fails, the job pauses instead of spending attempts. The FR-002, FR-003 and FR-004 outcomes depend only on the files and are stored as final at once.
- FR-016. Karaoke first: the job sends no request while a karaoke alignment is queued or running in the server's align queue, and never more than one at a time. It also starts no item while the transcode scheduler has jobs queued (as the styling job does, V34).
- FR-016a. The backfill orders film libraries before TV libraries (Alison, 2026-11-12T19:15Z), and other video libraries last.
- FR-017. An item a viewer opens is put ahead of the backfill (a small deduped FIFO, as the styling job's on-demand queue).
- FR-018. Without `[lyrics].aligner_url` the job does not run; tracks have no row and are served as today.

### Observability

- FR-019. `pharos_subtitle_check_total{verdict,reason_class,stage}` counts every finished check. `reason_class` is a bounded `label()` (`fits`, `offset`, `ratio`, `decoy_level`, `segmented`, `language_mismatch`, `script`, `language_untagged`, `too_few_lines`, `aligner_error`, `inconclusive`).
- FR-020. One log line per track, `subtitle check`, with the item id, track source and index, languages, verdict, the full reason, every probe and decoy score, the offset and ratio tried at each stage, aligner time and coarse-search time. Success and failure carry the same fields (ODD symmetry).
- FR-021. `pharos_subtitle_check_aligner_seconds` (histogram) records aligner time per check.
- FR-022. Karaoke contention needs a signal that does not exist today: `pharos_lyrics_align_wait_seconds` (histogram, no labels, buckets up to 1 h) records each karaoke job's time from queued to sent to the aligner. It ships first, on its own, so SC-003 has a baseline from before the job runs. It cannot see a karaoke request that is sent at once but then waits inside the aligner behind a check request (039 R5), so SC-003 also reads the existing `first_line_ms` field of the `lyrics: aligned` log line (queue to first timed line, which includes that wait). `first_line_ms` is only set when the aligner streams its answer (ADR-0121); a whole-JSON answer leaves it out, and `eta.actual_ms` cannot stand in because it includes the job's own compute. So SC-003, and with it `[subtitle_check].apply`, needs a streaming aligner; with a whole-JSON aligner the contention is unmeasured and the job stays log-only.

## Non-goals

- Retiming each line on its own (as karaoke does for songs). A constant offset and a speed ratio cover the common cases at about a fifteenth of the cost.
- Correcting `segmented` tracks (a different cut, removed ad breaks).
- Checking translated subtitles for fit. The coarse search could still find their offset; that is a possible follow-up, not part of this version.
- Non-Latin same-language tracks (needs a romaniser in the aligner).
- Writing any media file or sidecar, or renaming swapped videos.

## Success criteria

- SC-001. On a calibration set of at least 50 tracks with known answers, no right track gets `wrong_video`, and every known swapped or wrong track gets `wrong_video` or `unknown`.
- SC-002. A track shifted by any offset within ±120 s, or timed at 25 fps for a 23.976 fps video, gets `retimed` and plays within 100 ms of the right times (median over its probes).
- SC-003. Karaoke waits do not rise while the backfill runs: neither the p95 of `pharos_lyrics_align_wait_seconds` (server queue) nor the p95 of `first_line_ms` from `lyrics: aligned` (queue to first timed line, including any wait inside the aligner) against the week before the job ran.
- SC-004. Aligner time per checked track is at most 12 s at p95 on the GPU; server RSS rises by no more than 20 MB while the job runs.

## Open questions

None open. Decided 2026-11-12: films first (FR-016a); flagged tracks go to the admin list, with one Activity entry per pass (FR-011a).
