# ADR-0124: karaoke lines are placed by what is sung, not by their lyric stamps

- **Status:** Proposed
- **Date:** 2026-11-14T07:55:00Z
- **Deciders:** Alison (chose the approach 2026-11-14T06:15Z)

## Context

The aligner (ADR-0116) times each lyric line only inside a window around its synced-lyrics stamp. Downloaded stamps are often seconds wrong, worst around instrumental breaks, and the aligner then places every word on the wrong audio with believable spacing (B1402). Per-line scores do not flag it.

On a private regression set, a whole-song forced alignment with the letter model alone fixed gaps but slid repeated choruses by 16-46 s: the letter model cannot tell which line is sung where in a dense mix. A speech recogniser on separated vocals can.

## Decision

We will place each karaoke line from the singing:

1. Run today's per-line pass first and keep it as every line's fallback; stream it as today (ADR-0121).
2. Separate the vocals, recognise them with word times, and match transcript words to lyric words in lyric order. A line with at least half its words matched (minimum 2), inside a span capped by its stamp slot, is an anchor.
3. Refine each anchor's word starts letter by letter with the existing CTC model within ±1.5 s.
4. Time anchored and between lines together in one banded joint pass over the anchored span. Drop an anchor that is more than 5 s off both neighbours and leaps a sung line. Take in a kept edge line that would overlap the span, keeping the last feasible answer if that fails. Lines not bounded by anchors keep today's times.
5. Send a corrected event for each changed line, and the whole answer last, with an additive `placement` per line. `aligner_version` becomes 6.
6. Any stage failure keeps today's answer and records why.

## Consequences

- Lines follow the singing where the recogniser hears them; where it doesn't, they are no worse than today.
- A song costs more GPU time (about 15-50 s on the desktop GPU against ~5 s) and needs two more models (ADR-0125).
- First lines still arrive early, but some may be corrected after they were shown.
- Songs already aligned change only when the operator re-aligns them.

## Alternatives considered

- Re-search low-scoring runs between confident lines: moved a correctly stamped line 8 s.
- Per-line windows clamped between anchors (first plan): a line slid inside its band and the clamp spread the error to the next line.
- Whole-song letter alignment with a gap penalty and stamp band: repeated choruses landed anywhere in a 30 s range.
- The same on separated vocals: more confident, no better placement.

## References

- specs/042-karaoke-recogniser-alignment/spec.md, plan.md
- ADR-0116, ADR-0121, ADR-0125
