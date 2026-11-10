# ADR-0109: Sung subtitle lines are timed by the aligner when it hears their words, else paced

- **Status:** Proposed
- **Date:** 2026-11-10T11:40:00Z
- **Deciders:** Alison Jenkins

## Context

The styled track gives a karaoke fill only to lines whose source carries
`\k` syllable timings (spec 028 FR-007), and only inside an intro or outro.
Most opening and ending songs are subtitled without `\k`, so they show as
plain dialogue. Alison asked for karaoke on those songs, using the music
aligner (ADR-0066).

The aligner times words that are sung. Many song subtitles are not the words
that are sung: an English translation over Japanese singing is the usual
case for subbed anime. Forced alignment still returns a time for every word
of a translation, but the times are meaningless. Stream language tags cannot
tell the two apart, because a dub often keeps the original song.

## Decision

1. The aligner answers each line with a `score`, the mean CTC probability of
   its words' characters (Python service ALIGNER_VERSION 4). The probability
   is low when the text is not what is sung. The Rust service of spec 039
   sends no score yet; until it does, its songs are paced
   (`why=unscored`).
2. The styling job sends the aligner the lines that could take a fill:
   inside an intro or outro, not typeset, no `\k` of their own, and at least
   one Latin letter (`director::sung_candidates`). It waits for the answer,
   which is cached per item under the file's signature and the lines
   (`Aligner::sing`). A failure is logged, and the item is styled anyway.
3. The director takes the median score of the song lines. At or above
   `MIN_SUNG_SCORE` (0.3), the song's words are what is sung, and each line
   is timed from its word stamps. Below it, or when a line got no stamps, or
   when no aligner is configured, a line is paced: one syllable per word, in
   proportion to its vowel groups, over 90% of the line.
4. The timings travel in the plan (`LineEffect.syllables`), so serving
   compiles the same fill without asking again. A source's own `\k` still
   wins.

## Consequences

- `MIN_SUNG_SCORE` is a first guess that has not been measured on real
  songs. The per-item log line carries `sung_score`, so the threshold can be
  set from the scores of known sung and translated tracks.
- DIRECTOR_VERSION 6 re-directs every item. Each item with an intro or outro
  segment and Latin-letter lines makes one aligner request. ADR-0066 puts a
  song at seconds to tens of seconds of CPU; subtitle songs have not been
  measured. The job waits for each request in turn, and each answer is
  cached.
- A translated song gets a fill that keeps pace with the line but not with
  the singing. It reads as karaoke without claiming to follow the words.
- An aligner that is down while an item is styled leaves that item paced
  until the next director change. Nothing retries it sooner.
- Signals: `pharos_subtitle_sung_total{outcome}` (off, none, asked,
  failed) per item, `pharos_subtitle_karaoke_total{timing}` (source, aligned,
  paced) per karaoke line, and the log fields `sung`, `sung_score` and
  `karaoke`.
