# ADR-0147: Karaoke word times carry an aligner generation, and an older generation's are served until the new ones replace them

- **Status:** Accepted
- **Date:** 2026-11-20T11:30:00Z
- **Deciders:** Alison Jenkins

## Context

The server caches the aligner's word times for each song on disk
(`{id}.words.json`, ADR-0066). The cache key is a hash of a version number,
the song's path, size and mtime, and its lines. The version number
(`ALIGN_VERSION`) went up each time the aligner's answers changed, so old
answers missed the cache and every song was timed again.

A miss dropped the old answer. Until the song was played and timed again it
showed no word times at all, and when the new job failed it showed none for
the retry time. So a bump was costly, and songs timed by an older aligner
kept their old, worse times rather than risk that. One song timed on
2026-11-11 still carried times from before several aligner fixes on
2026-11-20.

## Decision

We will treat the version number as the **aligner generation** and keep each
song's older-generation answer in service until the current generation's
answer replaces it.

- `ALIGN_GENERATION` (was `ALIGN_VERSION`) is the generation. The key's shape
  is unchanged, so the key any older generation used can still be worked
  out from the same inputs. No file format, schema or migration changes.
- A song with no answer under this generation looks for one under each
  older generation, newest first, back to `OLDEST_SERVED_GENERATION` (4:
  generation 3 cached answers with no offset, B1007).
- Found, the song is queued for this generation and its old words are
  served as `Lookup::Refreshing`, with the "timing the words" estimate, so
  the client keeps asking. Each line this generation has streamed so far
  (ADR-0121) replaces the old line. When the new offset differs from the
  old one every line moves, so the old words stay whole until the new
  answer is stored.
- When the job fails, or the queue is full, the old words are served as
  `Lookup::Stale`, with nothing pending.
- This generation's answer, once stored, overwrites the old record.
- An older generation's mismatch verdict (ADR-0146) holds without timing
  the song again. It judges the lyric against what was heard, not how the
  words were timed.
- Sung subtitle lines (spec 040) fall back to an older generation's lines
  when the aligner fails.
- A bump that needs a newer aligner raises `MIN_ALIGNER_VERSION` with it,
  so an older aligner's answer fails the job and is never cached as the new
  generation (V1471). The old words then stay in service.

Re-timing is lazy: a song is timed again when it is played or is next in
the karaoke queue, as before. There is no sweep of the library.

## Consequences

- Bumping the generation is safe to do whenever the aligner improves. No
  song loses its words, and a song improves the next time it is played.
- Deploy the aligner the generation needs before the server that bumps to
  it. In the other order every re-time fails; the old words stay, so nothing
  breaks, but nothing improves until the aligner is deployed and the retry
  time passes.
- The banner says the words are being timed while a song is refreshed,
  even though it already shows words.
- Each lookup that misses this generation hashes up to one key per older
  served generation. The words file is still read once.
- Signals: `pharos_lyrics_words_total{answer="refreshing"}` and
  `{answer="stale"}`, and `generation` and `replaces` fields on the
  "lyrics: aligned" and "lyrics: alignment failed" log lines.

## Alternatives considered

- **Store the generation inside the record and key on content alone.**
  Changes every key at once, so every cached answer would need a migration
  or would read as a miss. The current key already names the generation.
- **Re-time the whole library in the background after a bump.** Takes the
  aligner's GPU from songs being played, for songs nobody may play. Lazy
  re-timing reaches the songs that matter first. A sweep can be added
  later.
- **Serve the old words with no streaming, swapping at the end.** Simpler,
  but the improved lines would only show after the whole song is timed.

## References

- ADR-0066 (word-time cache), ADR-0121 (streamed lines), ADR-0146
  (mismatch verdicts).
- `crates/pharos-server/src/services/lyrics/align.rs`: `ALIGN_GENERATION`,
  `OLDEST_SERVED_GENERATION`, `key_for`, `Aligner::stale`,
  `Aligner::refreshed`.
- Invariants V1470, V1471.
