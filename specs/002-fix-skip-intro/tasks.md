# Tasks: Skip Intro reaches the viewer

**Feature**: `specs/002-fix-skip-intro/` | **Branch**: `002-fix-skip-intro`
**Input**: [spec.md](./spec.md), [plan.md](./plan.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/](./contracts/), [quickstart.md](./quickstart.md)

**Tests are MANDATORY here** — the constitution makes TDD non-negotiable (V11: a
failing test precedes the implementation change) and ODD requires the signal to be
asserted and confirmed red when disarmed. Test tasks are not optional in this plan.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelizable — different file, no dependency on an incomplete task
- **[Story]**: US1 / US2 / US3, mapping to the spec's user stories
- Every task names its file path

## Path Conventions

Rust cargo workspace. Detection lives in `crates/pharos-transcode/`, the sweep and
its signals in `crates/pharos-server/src/segment_backfill.rs`, delivery in
`crates/pharos-server/src/api/jellyfin/system.rs`. All commands run inside the Nix
devShell (`nix develop --command …`).

---

## Phase 1: Setup

- [X] T001 Move the research fixture into the test tree: copy `specs/002-fix-skip-intro/fixtures-mushoku-s03.txt` to `crates/pharos-transcode/tests/fixtures/mushoku_s03_fingerprints.txt` and record its provenance (season, date dumped, `item_id kind hex` format, little-endian `u32`) in a header comment or sibling `README` at `crates/pharos-transcode/tests/fixtures/README.md`
- [X] T002 Add a fixture loader helper in `crates/pharos-transcode/tests/fingerprint_fixture.rs` that parses `item_id kind hex` lines into `(u64, String, Vec<u32>)`, decoding each 8-hex-char group with `swap_bytes()` to undo little-endian storage; no `unwrap` outside `#[cfg(test)]` opt-out

---

## Phase 2: Foundational (blocking prerequisites)

**Purpose**: establish the red test that every later task is measured against, and
settle the one open question from research R5. No user story can be verified before
these land.

- [X] T003 [P] Write the RED regression test in `crates/pharos-transcode/tests/intro_alignment_recall.rs`: load the fixture via T002, run `align::compare` over all 10 intro pairs and all 10 credits pairs with `AlignConfig::default()`, and assert intro matches reach the credits order of magnitude (≥6 of 10). Confirm it FAILS today with exactly 1 of 10 intro pairs — record the observed failure output in the test's doc comment
- [X] T004 [P] Add a stage-attribution assertion to the same test: for the failing intro pairs, assert the discard happens at candidate-shift discovery (zero shifts) and NOT at the duration bounds. This pins research R2's refutation so a future change cannot silently reintroduce a bounds-based explanation
- [X] T005 Answer research R5's open question by reading `crates/pharos-server/src/segment_backfill.rs`: determine whether a season is treated as "analysed" by row presence or by `schema_version`, and whether a season that produced zero rows is already retried on every 30-minute pass. Record the finding in `specs/002-fix-skip-intro/research.md` under R5 — this decides whether US3 needs code at all

**Checkpoint**: the failure is reproducible in CI, its stage is pinned, and the
re-analysis question is settled.

---

## Phase 3: User Story 1 — Skip a recurring opening (P1) 🎯 MVP

**Goal**: an episode whose season shares an opening carries an intro segment, so
the client offers Skip Intro over the correct window.

**Independent test**: the fixture test from T003 goes green, and a replay over the
Mushoku S03 intro fingerprints yields a consensus span matching the show's actual
opening length (≈90 s, per the credits-side control) rather than the current single
60.2 s outlier.

### Tests for User Story 1

- [X] T006 [P] [US1] Add a cost guard in `crates/pharos-transcode/tests/intro_alignment_recall.rs`: time a full 10-pair replay of both kinds and assert it completes well inside a generous ceiling (single-digit seconds), so an exhaustive shift search cannot regress the sweep unnoticed
- [X] T007 [P] [US1] Add a synthetic test in `crates/pharos-transcode/src/fingerprint/align.rs` tests module proving the mechanism directly: two fingerprints sharing a block whose points are all within `max_bit_diff` but never numerically equal within `index_shift` MUST still be matched. Confirm it fails on today's `candidate_shifts`

### Implementation for User Story 1

- [X] T008 [US1] Replace seeded shift discovery with a bounded exhaustive search in `crates/pharos-transcode/src/fingerprint/align.rs`: `compare` evaluates every shift over the two windows' overlap instead of only those `candidate_shifts` proposes. Keep `matches_at_shift`, `find_contiguous`, `bound_and_snap` and the best-span selection unchanged — the fix is confined to which shifts are considered
- [X] T009 [US1] Keep or delete `candidate_shifts` and `inverted_index` in `crates/pharos-transcode/src/fingerprint/align.rs` deliberately: if exhaustive search subsumes them, remove them and their tests rather than leaving dead code; if they are retained as a fast path, document why and prove both paths agree on the fixture
- [X] T010 [US1] Verify the existing synthetic suites in `crates/pharos-transcode/src/fingerprint/align.rs` and `season.rs` still pass unchanged — they encode the intro-skipper-derived semantics and B123's recall lesson, and none of them may be relaxed to make T008 pass
- [X] T011 [US1] Confirm the season consensus emits for the fixture season end to end: extend `crates/pharos-transcode/tests/intro_alignment_recall.rs` to run `detect_season_verbose` over the fixture's intro fingerprints and assert the verdicts move off `no_span`, with the emitted spans clustering (the ≈90 s opening), not scattering

**Checkpoint**: US1 is independently verifiable — the detector finds the opening it
was blind to, with no change to persistence, delivery or the client.

---

## Phase 4: User Story 2 — Silence stays explainable (P1)

**Goal**: after the fix, a season that yields no opening still says why, and the
distinction between "no shared opening" and "found and discarded" survives.

**Independent test**: with the fix in place, a season crafted to fail each way
produces the matching `Verdict`, and the per-season log line carries the values —
matched, agreeing, confidence — not a bare class.

### Tests for User Story 2

- [X] T012 [P] [US2] Assert the V75 contract in `crates/pharos-server/src/segment_backfill.rs` tests: a season detection run emits `pharos_segment_detect_total{kind,outcome}` for every episode and a per-season line carrying `episodes/emitted/low_confidence/few_agreeing/no_span` plus the `dropped` detail. Disarm the instrumentation once and confirm the test goes RED (contract R-OBS-4 in `specs/002-fix-skip-intro/contracts/detection-signals.md`)
- [X] T013 [P] [US2] Assert the outcome vocabulary is preserved in `crates/pharos-transcode/src/fingerprint/season.rs` tests: `Verdict::label()` remains the sole source of the metric label, the four labels stay distinct, and no new variant was introduced by T008 (contract R-OBS-1)

### Implementation for User Story 2

- [X] T014 [US2] Re-read the verdict path in `crates/pharos-transcode/src/fingerprint/season.rs` after T008 lands: confirm `matched = 0` still means "no comparison located a span" and has not become unreachable. If exhaustive search makes `no_span` vanish as a category, that is a signal change and must be stated, not absorbed
- [ ] T015 [US2] If and only if T003/T004 showed the cause was invisible from the existing log line, add the missing detail to `record_verdicts` in `crates/pharos-server/src/segment_backfill.rs` — carrying the offending value, never a bare class — and land it as its OWN commit ahead of T008 so it can ship and be read first (contract R-OBS-2, constitution ODD step 1)

**Checkpoint**: the diagnostic that made this bug findable is intact and tested.

---

## Phase 5: User Story 3 — Existing libraries recover (P2)

**Goal**: the 145 closing-only seasons gain their openings without an operator
deleting rows and without re-fingerprinting media.

**Independent test**: a season with cached fingerprints and zero intro rows is
re-compared on the next sweep and gains an intro, with no read of the source file.

### Tests for User Story 3

- [X] T016 [P] [US3] Add a test in `crates/pharos-server/src/segment_backfill.rs` proving re-analysis reuses cached fingerprints: a season whose `episode_fingerprints` rows are present and current MUST be re-compared without a source open. Assert the media path is never touched

### Implementation for User Story 3

- [X] T017 [US3] Implement the re-analysis trigger indicated by T005 in `crates/pharos-server/src/segment_backfill.rs`. If seasons with zero rows are already retried every pass, this task is a no-op and MUST be closed by stating that, with the evidence, rather than adding a mechanism nobody needs
- [X] T018 [US3] If T017 requires a version bump, bump ONLY the segment/detection schema version in `crates/pharos-server/src/segment_backfill.rs` (or its `pharos_core` constant), never the fingerprint schema version — invalidating `episode_fingerprints` forces a full-library NFS re-read for no benefit (research R5, data-model.md)
- [X] T019 [US3] Confirm idempotence and non-regression: `media_segments` has `PRIMARY KEY (item_id, kind)`, so re-analysis must REPLACE a range, never duplicate it, and a season that already has a correct opening must not lose or worsen it (FR-006). Assert both in `crates/pharos-store-sqlx/tests/media_segments.rs`

**Checkpoint**: existing libraries self-heal on the next sweep.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T020 [P] Run the full gate before committing: `nix develop --command just test`. `just test-postgres` is required only if an `sqlx::query*` string changed (T019 may touch one — check before skipping)
- [X] T021 [P] Run `nix develop --command cargo clippy --workspace --all-targets -- -D warnings`; pre-commit only runs rustfmt, and a clippy failure silently blocks the image publish
- [X] T022 [P] Record the latent defect found but deliberately not fixed here — `find_contiguous` returns only the longest run at a shift, which `bound_and_snap` may then reject outright, discarding a valid shorter run at the same shift — as a new task in `specs/001-pharos-baseline/tasks.md`, appending a fresh id (never renumbering)
- [X] T023 Append this defect to the bug ledger `specs/001-pharos-baseline/bugs.md` with cause and fix, using the next free id after B129, and add the invariant it is now guarded by to `specs/001-pharos-baseline/invariants.md` after V81: shift discovery and point acceptance must use the SAME notion of similarity — a fuzzy acceptance test seeded by an exact-match index can find nothing where thousands of near-matches exist
- [X] T024 Verify in production by query, not assertion (constitution ODD step 5). After deploy and one sweep pass, run the three checks in `specs/002-fix-skip-intro/quickstart.md` §5 — the recall-by-kind PromQL, the closing-only SQL (145 before), and the named-season LogQL — and report the ACTUAL output
- [X] T025 Close the loop on the reported symptom: play Mushoku Tensei S03 on the Google TV app, confirm Skip Intro appears over the same window the browser shows, and record the result in `specs/002-fix-skip-intro/research.md`
- [ ] T026 Update SC-001 in `specs/002-fix-skip-intro/spec.md` from the provisional 80% to the measured figure once T024 reports the post-fix verdict split across the 145 seasons (research R4's open item)
- [X] T027 Check episode `3096759618643281933` (Mushoku S03E02, matches nothing in either kind) against the known zeroed/corrupt-file list before accepting it as normal; note the outcome in `specs/002-fix-skip-intro/research.md`

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (T001–T002)**: no dependencies
- **Foundational (T003–T005)**: needs Setup. **Blocks every user story** — the red
  test is the measurement everything else is judged by
- **US1 (T006–T011)**: needs Foundational. No dependency on US2 or US3
- **US2 (T012–T015)**: needs Foundational. T014 reads the post-T008 state, so it
  follows US1; T015 (if needed at all) must land BEFORE T008
- **US3 (T016–T019)**: needs Foundational (T005 specifically) and the US1 fix to be
  worth running
- **Polish (T020–T027)**: needs the stories it verifies

### Critical ordering constraint

T015 is instrumentation. If T003/T004 show the cause was not visible from the
existing signal, T015 ships **first, as its own commit**, ahead of the fix — that is
constitution ODD step 1, and the reason it appears in a later phase is only that its
necessity is not known until Foundational completes.

### Story Independence

- US1 delivers the user-visible fix alone and is the MVP
- US2 protects the diagnostic; it can be verified against the fixed or unfixed code
- US3 is a recovery pass; without it the fix applies only to newly analysed seasons,
  which would leave the reported symptom in place for the shows that prompted it

### Parallel Opportunities

- T003 + T004 — same file, different assertions; write together, land together
- T006 + T007 — different files, no shared state
- T012 + T013 — different crates
- T020 + T021 + T022 — independent verification and bookkeeping

## Implementation Strategy

**MVP = Phase 1 + Phase 2 + Phase 3 (US1)** — T001 through T011. That is the whole
user-visible defect: the detector stops being blind to openings. Everything after is
about not going blind again (US2) and about the library already in the ground (US3).

**Incremental delivery**:

1. Setup + Foundational → the failure is reproducible in CI and its stage is pinned
2. US1 → Skip Intro works for newly analysed seasons; deployable and useful alone
3. US2 → the diagnostic is proven intact under test
4. US3 → the 145 existing seasons recover on the next sweep
5. Polish → verified by query in production, ledger updated, latent defect logged

**Commit discipline**: atomic — each commit does exactly one thing and reverting it
alone leaves the tree compiling. At minimum: fixture + red test; (optional
instrumentation); the alignment fix; the re-analysis trigger; the ledger entries.
Never squash.

## Notes

- The fix is small and localized. The task count is dominated by proving it, not by
  writing it — which is the correct ratio for a defect that hid behind a plausible
  wrong explanation for as long as this one did.
- Do not relax any existing synthetic test to make T008 pass. They encode semantics
  ported from the intro-skipper plugin and B123's recall lesson; a test that must
  change is a finding to report, not an obstacle to clear.

---

## Outcome (2026-07-27)

Closed. The user-visible defect was **B130** — `MediaSource.HasSegments` hardcoded
`false`, which made jellyfin-web skip the segment fetch entirely. Verified on the
TV by the user: starting Fringe S01E06 from the beginning shows the Skip Intro
prompt at 5:22, as the crossing-only ExoPlayer trigger predicts.

**B131** (shift discovery) was a real, separate blind spot found on the way and is
fixed under the same PR. It did not cause the report.

Tasks closed as not-needed, with reasons:

- **T015** — not needed. The existing V75 record already named the cause
  (`0 matched / 0 agreeing`), which is how the consensus layer was exonerated in
  one step. No instrumentation commit was required ahead of the fix.
- **T017 / T018** — `SEGMENT_DETECT_VERSION` already existed for exactly this
  (B123), so re-analysis was a one-line bump; no new mechanism.
- **T026** — SC-001's 80% target is moot. The premise that the 145 closing-only
  seasons are mostly fixable did not survive R7: the reported season genuinely has
  no consistent opening, and that is the expected state for a good fraction of the
  145.
- **T020 / T021** — `just test` 1759 passed / 33 skipped; clippy clean.

Shipped as PR #122.
