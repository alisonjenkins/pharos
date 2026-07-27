# Implementation Plan: Skip Intro reaches the viewer

**Branch**: `002-fix-skip-intro` | **Date**: 2026-07-27 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/002-fix-skip-intro/spec.md`

## Summary

Skip Intro never appears because, for the affected seasons, no intro segment
exists to serve. Delivery (`GET /MediaSegments/{id}` → `MediaSegmentDto`) is
verified healthy and shared with Outro, which works. The defect is in the
detection arm: on Mushoku Tensei S03 the intro run returned `no_span` for all
four episodes (`0 matched / 0 agreeing / conf 0.00`) while the credits run on the
same four files emitted 3 of 4.

`no_span` with `matched = 0` means **every pairwise `align::compare` returned
`None`** — not a consensus/threshold rejection. The consensus layer
(`detect_season_verbose`) is therefore not implicated; the alignment layer or the
window it is handed is.

The plan is: reproduce offline from the fingerprints already in the database (no
media re-read), identify which stage discards the match, fix it under test, then
re-analyse the 145 closing-only seasons.

## Technical Context

**Language/Version**: Rust stable (workspace toolchain, Nix devShell).

**Primary Dependencies**: `rusty-chromaprint` (fingerprint, `preset_test2`),
`pharos-transcode::fingerprint::{align, season}` (pure, dependency-free
alignment + consensus), `pharos-server::segment_backfill` (windowing, caching,
persistence, instrumentation), `pharos-core::MediaSegmentStore`.

**Storage**: Postgres in production. Two tables carry everything this feature
needs, both already populated:
- `episode_fingerprints (item_id, kind, points bytea, schema_version)` — the
  affected episodes hold ~5.6 KB of intro points each (≈1412 points ≈ 350 s at
  the 0.248 s hop), so the fingerprints are present and plausibly sized.
- `media_segments (item_id, kind, start_ms, end_ms, detector, confidence,
  schema_version)` — `PRIMARY KEY (item_id, kind)`.

**Testing**: `cargo nextest run --workspace`. The alignment and consensus layers
are pure and already unit-tested on synthetic vectors; this feature adds a
**real-fingerprint regression fixture** so the failing season is reproducible in
CI without media or a database.

**Target Platform**: Linux server; detection runs in the in-process
`segment_backfill` sweep behind the shared background-I/O gate.

**Project Type**: Rust cargo workspace — changes land in `pharos-transcode`
(detection) and `pharos-server` (windowing / re-analysis / signals).

**Performance Goals**: no regression to the sweep's cost per season. Alignment is
`O(n²)` pairwise over a season; any widened search must stay within the same
order. Detection must not degrade concurrent playback (V34/V35 gate, FR-009).

**Constraints**: V17 (no `unwrap`/`expect`), V15 (tracing only), V75 (a
detector's silence must be explainable), V11 (failing test first), V34
(background source reads hold a `bg_io` permit).

**Scale/Scope**: 987 seasons; 481 with an intro, 506 with a closing, **145
closing-only**. Re-analysis touches at most those 145 plus any newly failing
season.

**NEEDS CLARIFICATION**: none. The one soft target is SC-001's 80% figure, which
research task R4 converts into a measured number.

## Constitution Check

*GATE: checked before Phase 0 and re-checked after Phase 1.*

| Gate | Status | Note |
|------|--------|------|
| III — TDD, failing test first | PASS (planned) | R1 produces a real-fingerprint fixture that reproduces `no_span`; it goes red before any alignment change |
| III — ODD, name the query first | PASS | The proving query already exists and was used to diagnose: `{namespace="pharos",container="pharos"} \|= "detection verdicts"` plus `sum by (kind,outcome) (pharos_segment_detect_total)`. No new instrumentation is a prerequisite to the fix |
| III — instrument decisions with the offending value | REVIEW | `no_span` is recorded without the alignment inputs that produced it. If R1 shows the reason is invisible from the log alone, the first commit adds that detail, separately (see `contracts/detection-signals.md`) |
| IV — no panic / no unwrap | PASS | Pure code paths; fixture parsing lives in `#[cfg(test)]` where the lint is opt-out |
| V — types over conventions | PASS | No new state; a widened search returns the same `MatchResult` |
| V75 — silence is explainable | STRENGTHENED | This feature exists because V75's record made the cause findable; it must remain true after the fix |
| Metric labels bounded + stable | PASS | `Verdict::label()` stays the only source; a new variant is a dashboard change and must be justified, not incidental |

No violations to justify. One REVIEW item, resolved by R1's outcome.

## Phase 0: Research

**Complete.** Recorded in [research.md](./research.md), and it **refuted this
plan's own leading hypothesis (R2)**. R1 was run against production fingerprints:
9 of 10 intro pairs produce **zero candidate shifts**, so `compare` never builds a
span and never reaches the duration bounds R2 suspected. The cause is that shift
discovery matches points by exact value (±2 as an integer addend) while acceptance
matches them by Hamming distance ≤6 — a pair can share thousands of near-identical
points and still seed no shift. The fix is a bounded exhaustive shift search inside
`align::compare`; every downstream stage is unchanged.

Tasks as originally posed:

- **R1 — Reproduce offline.** Dump the five affected episodes' `intro` and
  `credits` fingerprints from `episode_fingerprints`, replay `align::compare`
  over every pair for both kinds, and record where the intro pairs die: no
  candidate shifts, no contiguous run, or a run rejected by `bound_and_snap`.
  Decisive, and needs no media.
- **R2 — Test the leading hypothesis.** `find_contiguous` returns only the
  *longest* run at a shift; `bound_and_snap` then rejects it outright when it
  exceeds `max_duration` (120 s), and `compare` moves to the next shift. A valid
  ~90 s opening sitting at the same shift, as a shorter run, is never considered.
  The intro window is up to 10 minutes (25% of runtime, capped) against the
  credits window's 450 s, and near-silence matches readily across a long head —
  so a spurious over-long run is far likelier on the intro side. That asymmetry
  fits the evidence exactly: same files, same detector, credits fine, intro
  `matched = 0`.
- **R3 — Enumerate the alternatives** so the fix is chosen, not assumed: window
  length and start; `max_bit_diff` / `index_shift` sensitivity; the
  `snap_start_secs` interaction; whether n=4 is simply too small a season;
  whether a stale `schema_version` is in play (observed: version 1 uniformly, so
  no).
- **R4 — Size the population.** Across the 145 closing-only seasons, classify how
  many fail the same way (`matched = 0` on intro) versus genuinely having no
  shared opening. Converts SC-001's assumed 80% into a measured ceiling.
- **R5 — Re-analysis trigger.** Establish how a season is currently considered
  "analysed" and what makes it eligible again, so FR-005 is met without an
  operator deleting rows and without re-fingerprinting media that is already
  cached.

## Phase 1: Design & Contracts

Artifacts:

- [data-model.md](./data-model.md) — the two persisted tables, the in-memory
  detection types, and the states a season moves through.
- [contracts/media-segments.md](./contracts/media-segments.md) — the client-facing
  wire contract. Unchanged by this feature; stated so a regression is visible.
- [contracts/detection-signals.md](./contracts/detection-signals.md) — the
  observability contract: the metric, its label set, and the per-season log line.
  This is the interface the fix is verified through.
- [quickstart.md](./quickstart.md) — reproduce the failure, run the fixture,
  verify by query after deploying.

### Post-design Constitution re-check

Unchanged: PASS, with the single REVIEW item deferred to R1's outcome. No new
persistent state, no new wire field, and no new metric label unless R1 proves the
existing `Verdict` set cannot express the cause — in which case that is itself the
first, separate commit.

## Project Structure

### Documentation (this feature)

```
specs/002-fix-skip-intro/
├── spec.md
├── plan.md              # this file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── media-segments.md
│   └── detection-signals.md
└── checklists/
    └── requirements.md
```

### Source Code (repository root)

```
crates/pharos-transcode/src/fingerprint/
├── align.rs      # compare / find_contiguous / bound_and_snap — leading suspect
└── season.rs     # consensus + verdicts — exonerated by matched = 0
crates/pharos-server/src/
└── segment_backfill.rs            # windows, fingerprint cache, persistence, V75 signals
crates/pharos-server/src/api/jellyfin/system.rs   # delivery — verified healthy
```

The regression fixture lands beside the code it guards, under
`crates/pharos-transcode/tests/`, with the dumped points as a compact data file.

## Complexity Tracking

No constitutional violation requires justification. Two judgement calls to record:

| Decision | Why | Alternative rejected |
|----------|-----|----------------------|
| Reproduce from stored fingerprints, not by re-reading media | The fingerprints are the detector's actual input, are already persisted, and turn the failure into a pure, CI-runnable test | Re-fingerprinting the source needs NFS reads behind the bg_io gate and cannot run in CI |
| Fix in the alignment layer if R1 confirms it, rather than widening or shortening the window | A window change alters cost for every season and would mask rather than fix a discard bug | Deferred to R1; if R1 exonerates alignment, the window becomes the next candidate |
