# ADR-0122: The lyric aligner checks subtitle tracks against their video, and fixes are applied when serving

- **Status:** Proposed
- **Date:** 2026-11-12T19:20:00Z
- **Deciders:** Alison Jenkins

## Context

- Pharos serves whatever text subtitle tracks sit beside or inside a video. It cannot tell a track that belongs to another video (a swapped pair of episode files was found by hand on 2026-11-11), and a mistimed track can only be corrected per viewer with the player's delay; a speed drift cannot be corrected at all.
- The Rust aligner (ADR-0116, spec 039) already runs on the cluster's GPU for karaoke, already reads video files for sung anime lines (B1212), returns a per-line `score` and finds a whole-file offset within ±8 s.
- A read-only spike on 2026-11-12 (spec 040) showed right English text scoring a median 0.90 against 0.25 for decoy text and 0.27 for the swapped episode's text, with a per-track rule separating every track; offsets within ±8 s were found to 35 ms; a +20 s offset or a 25/23.976 speed ratio was not found and looked like a wrong track; translated text (Japanese audio, English text) gave no signal.
- RAM is scarce on the node, and the GPU is shared with karaoke.

## Decision

We will add a background job to the server that checks each text subtitle track with the existing aligner service, and serve corrections at delivery time.

1. **No new service or model.** The job calls the aligner's existing `POST /align` with about 12 probe cues per track and, in a second request, decoy text in the same windows. The aligner's contract does not change.
2. **Same-language, Latin-script tracks only** get a fit verdict. Other tracks get `unknown` and no correction.
3. **Verdict per track, never per cue:** median and hit-count against configurable thresholds, plus a margin over the decoys.
4. **A track is called `wrong_video` only after a wide search fails:** offsets within ±120 s from a CPU speech-activity match (no aligner time) and the common film/PAL speed ratios.
5. **Corrections are a constant offset and a speed ratio,** stored per item and track source, applied when the server serves SRT, WebVTT or ASS. Media files and sidecars are never written. `?raw=1` serves the original.
6. **Karaoke goes first:** the job sends nothing while a karaoke alignment is queued or running, and one request at a time.
7. **Surfacing is off until calibrated:** `[subtitle_check].apply` defaults to `false`; the job logs every score so thresholds can be calibrated by query first.

## Consequences

- Wrong-video tracks and swapped files become visible to the admin and the viewer, and common offsets and drifts are fixed for every viewer at once.
- About 8 s of GPU time per track for a first pass over the library, then only new or changed files; no measurable RAM.
- The verdict depends on thresholds measured on a small sample; until calibration, nothing is surfaced.
- Translated subtitles, non-Latin same-language subtitles and tracks timed for a different cut are not fixed. Their verdict is `unknown` or `segmented`.
- A karaoke song that arrives while a check request runs waits for that request, about 4 s on the GPU.
- Served subtitles can differ from the files on disk; `?raw=1` and the "(synced)" label make that visible.

## Alternatives considered

- **Retime each line, as karaoke does.** About fifteen times the aligner time, and offsets and speed ratios cover the common cases.
- **Write corrected sidecars next to the media.** The media mount is read-only, other tools own those folders, and a wrong correction would be destructive.
- **An outside tool (ffsubsync, alass).** It fixes timing from speech activity but gives no confident wrong-video verdict, and adds a Python or separate binary to the image. The coarse search in decision 4 borrows its idea in Rust.
- **A speech-recognition model to compare transcripts.** A second large model on a shared GPU, for a test the aligner's scores already pass.

## References

- Spec 040 (`specs/040-subtitle-check/`).
- ADR-0116 and spec 039 (the Rust aligner), ADR-0067 (anime styling job, the job's mould), B1212 (sung lines sent to the aligner), B936 (whole-file offset).
