# ADR-0146: A lyric that was not sung is replaced once or shown unsynced

- **Status:** Accepted
- **Date:** 2026-11-19T12:00:00Z
- **Deciders:** Alison

## Context

Karaoke word times come from aligning a song's lyric text with its vocals.
The text comes from beside the file, from its tags, or from an online
provider (LRCLIB). A provider can hold another song's text under a song's
title and artist. The aligner then anchors few lines, falls back to a
guessed placement, and the server served those guessed words as synced
karaoke that never met the singing (B1479).

The aligner already transcribes the vocals to anchor lines. The share of
the lyric's word pairs found in that transcript tells a wrong lyric
(0.107 on the case found) from the right one (0.917 on the same
transcript). A song the recogniser barely heard also scores low, so the
number of words heard has to be counted too.

LRCLIB's `/api/search` returns up to 20 records for a title and artist,
with each record's length and synced text, and has no further pages. A
search that finds nothing usable says so of the records returned, not of
everything LRCLIB holds.

## Decision

We will judge every alignment's lyric against its transcript and never
serve a lyric that was not sung as synced karaoke (V1468).

- The aligner computes `text_recall` with
  `pharos_core::lyrics_text::bigram_recall`, shared with the server so
  both read one measure. Under 0.4 of the lines anchored, under 0.3
  recall and at least 100 words heard, it falls back as
  `lyrics_mismatch` and puts what it heard in the answer's `check`.
  It does not choose another lyric: it knows nothing of providers.
- The server keeps the verdict and the transcript in the words cache.
  `LyricsFinder` searches the provider once per song as tagged, keeps the
  records within 2 s of the song's length, scores the five closest
  against the transcript, and picks the best at 0.6 recall or more. The
  record that failed, and any copy of its text, is never picked.
- A pick replaces the lyric in the memo and the disk cache with the
  search marked done, and is aligned in the same request. With no pick,
  or a lyric from beside the file or its tags, the lines are served
  without times.
- On a song's first play its word times stream as they are timed,
  before the text check has run, as every other song's do. Once the
  aligner's verdict is a mismatch, the song is never served synced again.
- `ALIGN_VERSION` is not bumped: that would align the whole library
  again. Songs are judged as they are next aligned.

## Consequences

- A wrong online lyric is fixed without anyone editing a file, when the
  provider's search returns a record that passes the selection above. A
  right record the search leaves out of its 20, or one that misses a
  threshold, is not found, and the song is served without times.
- One provider search per affected song, ever; the disk record's mark
  survives restarts, and a retag searches again.
- The first play of a wrong lyric can show karaoke words that miss the
  singing until the aligner's verdict arrives. Holding them for the check
  would leave every song's first play without words for most of its
  length, which is the worse trade.
- A song the listener gave a wrong local lyric shows plain text rather
  than karaoke until the file is fixed. That is their file to change.
- Signals: `pharos_aligner_text_recall`,
  `pharos_lyrics_repick_total{outcome}` and
  `pharos_lyrics_unsynced_total{reason="lyrics_mismatch"}`.
- The thresholds rest on few measured songs. The recall histogram shows
  whether they hold across the library.

## Alternatives considered

- **Let the aligner pick another record.** It would need provider access
  and the server's cache. Kept in the server, which already owns both.
- **Serve the fallback words anyway.** That is the bug.
- **Re-pick by record id.** Cached records carry no id, and the failed
  text always matches the cached lines, so excluding by text covers both.
- **Bump `ALIGN_VERSION`.** Correct but costs a full library re-align.

## References

- B1479, V1468 in `specs/001-pharos-baseline/`.
- `crates/pharos-align/src/check.rs`,
  `crates/pharos-server/src/services/lyrics/repick.rs`.
