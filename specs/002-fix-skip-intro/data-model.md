# Data Model: Skip Intro reaches the viewer

**Date**: 2026-07-27 | **Plan**: [plan.md](./plan.md)

This feature adds **no new persisted entity and no new field**. Everything below
already exists; it is recorded so the fix's blast radius is explicit and so a
migration can be recognised as out of scope.

## Persisted entities

### `episode_fingerprints`

The detector's cached input — the audio fingerprint of one window of one episode.

| Column | Type | Notes |
|--------|------|-------|
| `item_id` | bigint | media id; part of PK |
| `kind` | text | `intro` \| `credits`; part of PK |
| `points` | bytea | little-endian `u32` per fingerprint point, ≈0.2476 s of audio each |
| `schema_version` | bigint | currently `1` for every row |

- PK `(item_id, kind)` — one fingerprint per window per episode.
- Rows are written once and reused; a cache hit skips the (NFS) source read.
- **Unchanged by this feature.** The comparison changes, the input does not, so
  the version MUST NOT be bumped in a way that invalidates these rows (R5) — a
  full-library re-fingerprint would cost NFS reads for no gain.

### `media_segments`

The detector's output — a labelled range on one episode, and what clients are
ultimately served from.

| Column | Type | Notes |
|--------|------|-------|
| `item_id` | bigint | media id; part of PK |
| `kind` | text | `Intro` \| `Outro`; part of PK |
| `start_ms` / `end_ms` | bigint | on the episode's own timeline, window offset already applied |
| `detector` | text | e.g. `chromaprint` — how the range was found |
| `confidence` | double | agreement fraction from the season consensus |
| `schema_version` | bigint | detection schema |

- PK `(item_id, kind)` — at most one intro and one closing per episode. A
  re-analysis that finds a better range **replaces** rather than accumulates.
- Note the case difference from `episode_fingerprints.kind` (`Intro` vs `intro`):
  the two columns are different vocabularies and must not be compared directly.

## In-memory detection types

| Type | Location | Role |
|------|----------|------|
| `EpisodeFingerprint { id, points, window_offset_secs }` | `fingerprint::season` | one episode's window, plus where that window's zero sits on the episode timeline (0 for intro, `duration − 450 s` for credits) |
| `AlignConfig` | `fingerprint::align` | `max_bit_diff` 6, `index_shift` 2, `max_time_skip` 3.5 s, `min_duration` 15 s, `max_duration` 120 s, `snap_start_secs` 5 s, `secs_per_point` ≈0.2476 |
| `Span { start, end }` / `MatchResult { lhs, rhs }` | `fingerprint::align` | a located range, per episode of the compared pair |
| `SeasonConfig` | `fingerprint::season` | `cluster_tolerance_secs` 3.0, `min_agreeing` 2, `min_confidence` 0.5 |
| `Verdict` | `fingerprint::season` | `Emitted` \| `NoSpan` \| `FewAgreeing` \| `LowConfidence` — **a metric label set**; see `contracts/detection-signals.md` |
| `EpisodeVerdict { id, matched, agreeing, confidence, span, verdict }` | `fingerprint::season` | the per-episode record V75 requires |
| `SeasonSegment` | `fingerprint::season` | an emitted consensus range, ready to persist |

The fix (R3) is confined to how candidate shifts are produced inside
`align::compare`. `MatchResult` and everything downstream of it keep their current
shapes, so `season.rs`, the persistence path and the wire path are untouched.

## Validation rules

Carried from the spec's functional requirements:

- A persisted range MUST satisfy `min_duration ≤ end − start ≤ max_duration`
  (FR-008 — a skip must land at the opening's end, not inside it or past it).
- A range's endpoints are on the **episode** timeline: the window offset is added
  once, in `detect_season_verbose`, and never again downstream.
- An intro starting at or before `snap_start_secs` snaps to 0.
- A season of fewer than 2 episodes yields no verdicts and no segments — there is
  no peer to agree with.
- An episode whose source cannot be fingerprinted is absent from the comparison
  set and MUST NOT prevent its peers from being analysed (FR-010).

## State transitions

A season, per kind:

```
unanalysed
  → fingerprinted        (points cached per episode; NFS read, bg_io gated)
  → compared             (pairwise align over the cached points)
  → { emitted | no_span | few_agreeing | low_confidence }   per episode
  → persisted            (emitted only)
```

Re-analysis re-enters at **compared**, reusing cached fingerprints (R5). The
transition that this feature repairs is `fingerprinted → compared`, which today
terminates at `no_span` for seasons where no exact-valued point pair exists to
seed a shift.
