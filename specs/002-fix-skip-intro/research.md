# Phase 0 Research: Skip Intro reaches the viewer

**Date**: 2026-07-27 | **Plan**: [plan.md](./plan.md)

All five tasks are resolved. R1 was run for real against production fingerprints
and it **refuted the plan's leading hypothesis (R2)**, so the fix direction below
is the measured one, not the assumed one.

## R1 — Reproduce offline: where does the intro comparison die?

**Decision**: reproduced. Every intro pair but one dies at **candidate-shift
discovery**, before any span is ever built.

**Method**: dumped `episode_fingerprints.points` for the five Mushoku Tensei S03
episodes (both `intro` and `credits`) as hex, decoded LE `u32` (the encoding
`postgres.rs` writes), and replayed `align::compare` over all 10 pairs of each
kind in a throwaway test in `pharos-transcode`. The dump is kept as
[fixtures-mushoku-s03.txt](./fixtures-mushoku-s03.txt) (`item_id kind hex`, one
line per row) so the fixture is reproducible without cluster access.

**Result** (hop 0.2476 s; intro 1412–1413 points ≈ 350 s, credits 1794–1796
points ≈ 445 s):

| Kind | Pairs matched | Detail |
|------|---------------|--------|
| intro | **1 of 10** | only 0×4, a 60.2 s span at 136.2 s |
| credits | 6 of 10 | 90.9 s span at ≈353.8 s — the real ending |

Stage attribution for the failing intro pairs:

```
0x1 -> None | shifts 0 | longest-any 0.0s | rejected: too_long 0 too_short 0
0x2 -> None | shifts 0 | ...
…9 of 10 identical…
0x4 -> Some(136.2..196.4) | shifts 1 | longest-any 60.2s
```

**`candidate_shifts` returned an empty vector.** With no shift to test, `compare`
never calls `matches_at_shift`, never builds a run, and never reaches
`bound_and_snap`. `matched = 0` in production is therefore literal: no comparison
produced anything to cluster.

**Rationale for trusting this**: the same replay reproduces the credits successes
(90.9 s at 353.8 s, agreeing with the persisted `media_segments` rows) and the
one production intro outlier, on the same inputs the sweep used.

## R2 — The `find_contiguous` / `max_duration` hypothesis: REFUTED

**Decision**: rejected. The plan supposed that a spurious over-long run (>120 s)
was being chosen as the longest at a shift and then discarded, hiding a valid
~90 s opening at the same shift.

**Evidence against**: across every failing intro pair, `too_long = 0` and
`too_short = 0` — there were no shifts at all to reject, over-long or otherwise.
The probe also enumerated *every* run per shift (not just the longest) and found
no in-bounds run hiding behind a longer one.

**Retained as a latent defect**: `find_contiguous` returning only the longest run
while `bound_and_snap` can reject it outright is still a real recall hazard — it
just is not this bug. It appears once in the credits data (`0x1`: longest-any
2.5 s, everything under `min_duration`), where it changes nothing. Worth a
separate task, not part of this fix.

## R3 — Why shift discovery fails, and the alternatives

**Decision**: the defect is a **mismatch between how shifts are discovered and how
points are matched**.

- Acceptance is *fuzzy*: two points match when `(a ^ b).count_ones() <= 6` — up to
  6 of 32 bits may differ.
- Discovery is *exact*: `candidate_shifts` builds `value → index` maps and probes
  `value ± index_shift` (±2 as an **integer addend**, the intro-skipper's
  `InvertedIndexShift`). A shift is only ever proposed when two points are
  numerically equal within ±2, which for a bit-packed chromaprint word means the
  two low bits and nothing else.

So a pair of episodes can share thousands of points within the 6-bit tolerance and
still yield **zero** candidate shifts, because not one pair of points was
numerically near-identical. That is exactly the observed state: 9 of 10 intro
pairs, 0 shifts, on a season whose opening is the same audio every episode.

Why the intro window is hit harder than credits is a secondary question and does
not need answering to fix this — but the likely reason is phase: the intro window
starts at container zero, and per-episode differences in audio start offset shift
the fingerprint hop grid, so identical audio quantizes to different words. The
credits window starts at `duration − 450 s`, and the encodes here happen to land
on a compatible phase often enough to seed 1–4 shifts.

**Alternatives considered**:

| Option | Verdict |
|--------|---------|
| Brute-force every shift in the overlap instead of seeding from an index | **Preferred.** ~1400 points per window → ≈2800 shifts × ≤1400 comparisons ≈ 4M `u32` XOR+popcount per pair, single-digit milliseconds. A 20-episode season is 190 pairs. Cost is bounded, deterministic, and removes the failure mode entirely rather than making it rarer |
| Widen `index_shift` (±2 → larger) | Rejected. Still exact-value seeding; a larger addend probes numerically-near words, not Hamming-near ones, so it does not address the mechanism. Also enlarges the index probe quadratically for no principled gain |
| Seed shifts from a coarse bucket (e.g. high bits / popcount class) | Viable fallback if brute force proves too slow on the largest seasons. Adds a tuning knob and a new failure mode (bucket collisions) that brute force does not have |
| Lengthen or move the intro window | Rejected as a fix. Changes cost for every season and would not create a shift where none exists |
| Loosen `max_bit_diff` | Rejected. Acceptance is not the failing stage |

**Decision**: replace or supplement seeded discovery with a bounded exhaustive
shift search, keeping every downstream stage (`matches_at_shift`,
`find_contiguous`, `bound_and_snap`, the season consensus) unchanged, so the fix
is one localized change with an existing test wall behind it.

## R4 — Population size

**Decision**: the DB-side baseline is measured; the per-season cause split is
deferred to implementation, where re-running detection produces it as a by-product.

Measured now (production Postgres, 2026-07-27):

| Metric | Value |
|--------|-------|
| Seasons with any segment row | 987 |
| …with an intro | 481 |
| …with a closing | 506 |
| **Closing-only (the defect's signature)** | **145** |
| Intro-only | 120 |
| Intro rows / closing rows | 6113 / 6746 |

Intro start times run 0 s – 577 s (median 112 s) and lengths 15 s – 120 s, so the
intro side is not uniformly broken — it works wherever shift discovery happens to
seed. That is consistent with a phase-luck failure and inconsistent with a
systematic window or wire fault.

**Open**: what fraction of the 145 genuinely have no shared opening. SC-001's 80%
target stays provisional until the re-analysis in R5 reports its own verdict
counts, which is the honest way to measure it.

## R5 — Re-analysis trigger

**Decision**: gate re-analysis on the detection schema version, not on operator
action.

`segment_backfill` re-scans on a 30-minute interval and no-ops fast for seasons it
considers analysed; `episode_fingerprints` and `media_segments` both carry
`schema_version` (currently `1` everywhere, uniformly — so no stale rows are in
play today).

The fingerprints themselves do not change under this fix — only the comparison
does — so re-analysis must **reuse the cached fingerprints and re-run only the
comparison**. That keeps FR-005 cheap: no NFS re-read, no bg_io pressure, and the
145 seasons recover on the next sweep. A version bump that invalidated
`episode_fingerprints` would force a full library re-fingerprint for no benefit
and must be avoided.

Detail to settle in `/speckit-tasks`: whether the segment schema version alone is
enough to re-drive a season, or whether "analysed" is currently inferred from the
presence of rows (in which case a season that produced zero rows may already be
retried every pass, and only the comparison fix is needed).

## R6 — Post-fix measurement: the shift fix is necessary but NOT sufficient

**Added 2026-07-27, after implementing the R3 decision.** Exhaustive shift search
landed and the synthetic mechanism test went green — but real intro recall on the
fixture **did not move**: still 1 of 10 pairs, verdicts unchanged. R3's diagnosis
was correct about the stage and incomplete about the cause.

Sliding the opening located in the one working pair (E04's 136.2–196.4 s block)
against every episode's own window:

| Episode | Best mean bit-distance | At | Points within 6 bits |
|---------|------------------------|-----|----------------------|
| E04 (ep0) | 0.00 | 136.2 s | 243 / 243 |
| E05 (ep4) | 5.83 | 159.2 s | 141 / 243 |
| E02 (ep1) | 15.00 | — | 0 / 243 |
| E03 (ep2) | 14.59 | — | 1 / 243 |
| E01 (ep3) | 15.04 | — | 0 / 243 |

A mean of ~15 differing bits of 32 is chance (16 is random). **That audio is not
in the first 350 s of three of the five episodes.** No comparison can find what
was never fingerprinted.

Loosening `max_bit_diff` does not help and actively harms: at 10–12 bits every
pair "matches" 30–140 s at arbitrary shifts, including credits pairs that are
genuinely unrelated. That is noise saturation, not recall.

Ruled out along the way:
- **Different audio track per file** — all five are `aac`, 2ch, 48 kHz, `jpn`
  default; E01/E02 additionally carry an `eng` track. The credits window of the
  same files matches at 6 bits with shift ≈0, so decoding and fingerprinting are
  consistent across the season.
- **Window phase** — intro windows all start at container zero, so they are
  phase-identical by construction. (Credits windows start at `duration − 450 s`
  and differ by up to 590 ms between episodes, yet still match — the fingerprint
  tolerates sub-hop phase differences.)
- **Duration mismatch** — all five episodes are 1420 s ±0.6 s.

**Remaining hypotheses, needing the media to separate:**
1. The opening sits **beyond the 350 s window** in those episodes (25% of runtime,
   capped at 10 min — 5 min 50 s here). A long cold open would put it out of reach.
2. Those episodes carry a **different or absent opening**.

**Blocked**: distinguishing these needs the source files, which are on the
cluster's NFS and not reachable from the dev host. Either a read-only mount or an
in-cluster probe is required, and both are infrastructure actions needing the
user's approval.

**Kept anyway**: the shift-discovery fix is a genuine correctness repair with a
synthetic proof, no regression on real credits data (still 6 of 10 at 91 s), and
no measurable cost. It removes a blind spot that would otherwise hide behind
whatever the window turns out to be.

## R7 — Media-level ground truth: this season HAS no consistent opening

**Added 2026-07-27**, from a read-only debug pod (`pharos-fp-debug`, linuxserver
jellyfin image, `pharos-media` PVC mounted read-only, deleted after use). Audio
was extracted with ffmpeg and cross-correlated locally — a stronger instrument
than the fingerprint, since it works on the waveform itself.

**Method control first.** The closing, which production detects successfully,
reproduces exactly: a 30 s reference from each episode's ending matches the
others at **0.98–1.00**, except E02, which matches nothing — the same odd file
production reports `no_span` for. So the extraction picks the same audio stream
the detector does, and the instrument is sound.

**The opening does not exist in three of the five episodes.** E04's opening
(136.2–196.4 s) searched across the **entire runtime** of every episode:

| Region searched | E01 | E02 | E03 | E04 | E05 |
|-----------------|-----|-----|-----|-----|-----|
| 0–900 s | 0.024 | 0.022 | 0.015 | **1.000** | **0.835** |
| 880–1280 s | 0.029 | 0.029 | 0.018 | — | — |
| 1250–1430 s | 0.026 | 0.021 | 0.027 | 0.026 | 0.026 |

~0.02 is noise. A sweep of 30 s references taken every 60 s across the first
10 minutes of *every* episode against *every* other found only one shared pair:
E04×E05. E01, E02 and E03 share no opening with anyone, including each other.

**So hypothesis 1 (opening beyond the window) is refuted, and the detector's
behaviour on this season is CORRECT.** Three episodes have no shared opening to
find; the two that do are a single agreeing pair, one short of `min_agreeing`,
which is deliberately not enough — a lone pair is the shape of a coincidence.

## R8 — The reported symptom is therefore NOT this season's detection

The viewer has played episodes that **do** carry an intro segment:

| Episode | Intro | Source |
|---------|-------|--------|
| Code Geass S02E12 | 112–200 s, conf 1.00 | detected (chromaprint) |
| Neon Genesis Evangelion S01E18 | 0–89 s | detected |
| Fringe S01E05 | 384–403 s | detected |
| Mushoku S03E01 | 81–166 s | **chapter** titled `Intro` |
| Mushoku S03E05 | 200–219 s | **chapter** titled `Intro` |

Mushoku S03E01 and S03E05 carry a chapter literally titled `Intro`, which
`classify_chapter_title` maps to a segment ahead of any detection. That matches
the wire evidence: the production request for S03E05 returned **351 bytes**
against **200 bytes** for S03E04 — two segments versus one.

So Skip Intro data reached the client for at least one of the very episodes in
the report, and the button still did not appear. The remaining candidates are
client-side: the Android TV app's per-segment-type action setting, or its
rendering of an Intro segment specifically. Library-wide there are 6113 intro
rows, so this is not a data drought.

**Next evidence needed**: the actual JSON body of
`GET /MediaSegments/{id}?includeSegmentTypes=Intro&includeSegmentTypes=Outro`
for `7093100027006938661` (S03E05, wire id `00000000-0000-0000-626f-c0e4ca0fee25`).
Fetching it needs an auth token, which this session is not permitted to read.

## R9 — RESOLVED: the defect was `HasSegments`, and the app was never broken

**2026-07-27.** Two independent findings closed this out.

**The server defect (ours).** `MediaSource.HasSegments` was hardcoded `false` —
added for the strict Kotlin decoder (B13), never populated. jellyfin-web's
`MediaSegmentManager.onPlayerPlaybackStart` reads it and returns BEFORE calling
`getItemSegments`:

```js
this.hasSegments = !!(t.MediaSource?.HasSegments);
if (this.hasSegments && serverId && sourceId) { … fetchMediaSegments(…) }
else console.info("[MediaSegmentManager] user has no media segment actions enabled")
```

So `/MediaSegments/{id}` was unreachable from every browser, for every item,
always — while answering correctly the whole time. Confirmed in production: a
browser played an episode whose intro is stored at 322.3–342.8 s and issued no
`/MediaSegments` request in the entire session. Fixed, with the advertisement
answered from the same two sources `build_media_segments` serves so the two
cannot disagree.

**The Android TV app is working as designed.** `PlaybackControllerHelper.kt`
schedules a single ExoPlayer `PlayerMessage` at the segment start:

```kotlin
.createMessage { _, _ -> fragment?.askToSkip(mediaSegment.end) }
// Segments at position 0 will never be hit by ExoPlayer so we need to add a minimum value
.setPosition(mediaSegment.start.inWholeMilliseconds.coerceAtLeast(1))
.setDeleteAfterDelivery(false)
```

It fires only on a FORWARD CROSSING of that position — not a periodic check. An
outro at ~22 min is always crossed; an intro at 3–6 min is missed whenever
playback resumes past it, which Jellyfin's own resume position causes routinely.
**Verified by the user**: starting Fringe S01E06 from the beginning, the Skip
Intro prompt appears at 5:22 as predicted.

Ruled out along the way: the undashed 32-hex `Id` (the SDK's UUID serializer
normalizes it), stale per-item caching (fetched fresh at `startItem`), any
Intro/Outro branch in the skip code, and any known 0.19.x issue.

**Consequence for the spec**: US1's premise — that the detector was blind — was
half right. It was blind in one specific way (R3, fixed), but the reported
symptom was the `HasSegments` gate. The zero-start intro concern (24% of rows)
is NOT a defect for either client: the TV app coerces to 1 ms and jellyfin-web
evaluates by position range.

## Incidental finding

Episode `3096759618643281933` (S03E02) matched **nothing** in either kind, in both
production and the replay — it is the `no_span` outlier in the credits run too.
One file differing from its peers is expected behaviour, not a defect, but it is
worth confirming it is not one of the known zeroed/corrupt library files before
treating it as normal.
