# Tasks: hardware encoding for CMAF renditions

**Input**: Design documents from `specs/003-cmaf-hw-affinity/`
**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/device-affinity.md](./contracts/device-affinity.md)

**Tests**: REQUIRED. Constitution principle III (test-first, then prove it by
query) and principle IV apply. Every gate task below must be seen to FAIL before
its rule exists — a green test that was never red proves nothing here, and this
feature re-enables the code path that caused issue #114.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: different file, no dependency on an incomplete task — safe to run in parallel
- **[US1/US2/US3]**: the user story the task serves

## Path Conventions

Single Rust workspace. Paths are repo-relative:
`crates/pharos-transcode/src/…`, `crates/pharos-cache/src/…`.

---

## Phase 1: Setup — the gate

**This phase can void the feature. Nothing else may start until T001 is green.**

- [X] T001 Measure R4 on the GPU host: encode two independent segment windows of one source on `nvenc:0` with identical settings via `crates/pharos-transcode/src/bin/transcode_tool.rs`, extract the `avcC` parameter sets from each, and byte-compare. Repeat once after restarting the worker process. Record the actual bytes and the verdict in `specs/003-cmaf-hw-affinity/research.md` under R4.
- [X] T002 (not triggered — T001 green) If T001 shows the parameter sets DIFFER: stop. Record the refutation in `specs/003-cmaf-hw-affinity/research.md`, mark this spec superseded, and append a note to `specs/001-pharos-baseline/tasks.md` T105 that the CPU-only rule in `crates/pharos-transcode/src/device.rs` is correct as written. Do not proceed to Phase 2.

**Checkpoint**: hardware proven self-consistent across processes, or the feature is dead.

---

## Phase 2: Foundational (blocking prerequisites)

**Purpose**: the rendition identity, and the cache safety that MUST land before
hardware becomes eligible. No user story can start until this phase completes.

- [X] T003 Add `RenditionKey` (derive from `TranscodeOptions` with `start_position_ticks` / `duration_ticks` excluded, per data-model.md) in `crates/pharos-transcode/src/options.rs`.
- [X] T004 Test in `crates/pharos-transcode/src/options.rs`: two options differing ONLY in start/duration produce the SAME key; options differing in any encode-affecting field (video, container, bitrate, audio index, burn index/flag, frame rate) produce DIFFERENT keys. Verify it fails if a field is dropped from the derivation.
- [X] T005 Add a `shared_init_fmp4(opts) -> bool` predicate in `crates/pharos-transcode/src/device.rs` expressing the hazard by CONTAINER contract, not by codec list (R6), and test that H264+Fmp4 and a hypothetical H265+Fmp4 both qualify while mpegts does not.
- [X] T006 (SUPERSEDED by R8 — a deterministic device makes cached bytes unambiguous; no cache change needed) — Include the producing device in the on-disk segment identity in `crates/pharos-cache/src/hls_cache.rs` (R3).
- [X] T007 (SUPERSEDED by R8 — no cache identity change, so no generation bump and no 40 GiB regeneration) — Bump `HLS_GEN_VERSION` in `crates/pharos-cache/src/hls_cache.rs` so the existing CPU-era cache is discarded before any hardware init can be served over it.
- [X] T008 (SUPERSEDED by R8 — nothing can cross pins when the device is a pure function of the rendition) — Test in `crates/pharos-cache/src/hls_cache.rs`: a segment cached as produced by device A is NOT returned for a lookup whose rendition is pinned to device B (contract test 4). Verify it fails without T006.

**Checkpoint**: identity and cache are safe. Hardware is still ineligible — nothing user-visible has changed yet, by design.

---

## Phase 3: User Story 2 — a rendition never mixes encoders (Priority: P1) 🎯 the guard

**Goal**: the one-encoder guarantee holds under load, before anything relaxes the
exclusion that currently provides it.

**Independent test**: dispatch many segments for one rendition key while the
pinned device is saturated and another device is free; every job reports the
pinned device.

**Sequenced FIRST despite US1 being the headline**: US1's enabling change (T017)
is unsafe until this exists. See Dependencies.

### Tests for User Story 2

- [X] T009 [US2] Contract test 1 in `crates/pharos-transcode/src/scheduler.rs`: with a synthetic device table, submit N jobs for one `RenditionKey` while the pinned device's permits are exhausted and CPU is free; assert every dispatch names the pinned device and none spills. Verify RED before T011.
- [X] T010 [P] [US2] Test in `crates/pharos-transcode/src/scheduler.rs` that a job whose options are NOT shared-init fMP4 still load-balances freely across devices (FR-006, contract test 3).

### Implementation for User Story 2

- [X] T011 (SUPERSEDED by R8 — no pin map; the device is computed, not stored) — [US2] Add the pin map (`RenditionKey → RenditionPin`) to the scheduler actor's own state in `crates/pharos-transcode/src/scheduler.rs` — actor-owned, no shared lock (constitution V).
- [X] T012 [US2] In `place()` in `crates/pharos-transcode/src/scheduler.rs`: for a shared-init fMP4 job, record the pin on first placement and restrict candidates to the pinned device thereafter; when its permits are busy, QUEUE rather than widen the candidate set.
- [X] T013 (SUPERSEDED by R8 — nothing to evict) — [US2] Add pin eviction on idle TTL in `crates/pharos-transcode/src/scheduler.rs` so a finished rendition does not hold a device preference indefinitely.

**Checkpoint**: the guarantee is enforced by adherence rather than by exclusion — still with hardware ineligible, so this is provably a no-op in production until Phase 5.

---

## Phase 4: User Story 3 — losing the device degrades safely (Priority: P1)

**Goal**: a lost pinned device fails loudly. Silent continuation on another
encoder IS issue #114.

**Independent test**: induce cooldown on the pinned device mid-rendition; the
request errors and no CPU dispatch occurs.

### Tests for User Story 3

- [X] T014 [US3] Contract test 2 in `crates/pharos-transcode/src/scheduler.rs`: with a rendition pinned to a hardware device, set that device into cooldown and submit the next segment; assert `SchedError` is returned and assert NO job is dispatched to CPU. Verify RED before T015.

### Implementation for User Story 3

- [ ] T015 [US3] On pinned-device cooldown or repeated transient failure in `crates/pharos-transcode/src/scheduler.rs`, invalidate the pin and return `SchedError::Failed` rather than re-placing on another device (R2, FR-004).
- [X] T016 (SUPERSEDED by R8 — a restart recomputes the same device, so there is no re-pin to test) — [US3] Ensure a subsequent submit for the same key after invalidation re-pins from scratch (the client has restarted the stream and re-fetched the init), with a test in `crates/pharos-transcode/src/scheduler.rs`.

**Checkpoint**: both #114 guards are in place and tested. Only now is it safe to let hardware in.

---

## Phase 5: User Story 1 — a group watch on browsers uses the GPU (Priority: P1)

**Goal**: the actual win — browser CMAF renditions reach the GPU.

**Independent test**: a browser CMAF H264 rendition dispatches to the hardware
device, and every one of its segments comes from that device.

### Tests for User Story 1

- [X] T017 [US1] Test in `crates/pharos-transcode/src/device.rs` that `eligible_for` on H264+Fmp4 now lists hardware FIRST then CPU (replacing today's `&[DeviceId::Cpu]` assertion in `h264_fmp4_cmaf_routes_cpu_only_but_mpegts_keeps_hardware`), and that mpegts H264 is unchanged.
- [X] T018 [US1] Update the existing test named `h264_fmp4_cmaf_routes_cpu_only_but_mpegts_keeps_hardware` in `crates/pharos-transcode/src/device.rs` — rename it and rewrite its comment to explain that the one-encoder guarantee now comes from the pin, citing issue #114 and this spec so the reasoning is not lost.

### Implementation for User Story 1

- [X] T019 [US1] Remove the `Some(VideoCodec::H264) if opts.container == Container::Fmp4 => false` arm from `device_supports` in `crates/pharos-transcode/src/device.rs`, replacing the exclusion with the `shared_init_fmp4` predicate used by the scheduler's pin logic.

**Checkpoint**: SC-001/SC-002 become measurable. This is the MVP boundary — the feature is functionally complete here.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [ ] T020 [P] Add `pharos_transcode_pin_total{outcome}` with the four bounded outcomes (`pinned`, `followed`, `queued_on_pin`, `invalidated`) in `crates/pharos-transcode/src/scheduler.rs`, and assert the label set is distinct in a test (metric labels are a dashboard contract).
- [ ] T021 [P] Add log lines on pin and on invalidation in `crates/pharos-transcode/src/scheduler.rs` carrying the rendition key hash, the device, and a reason that names the offending value — never a bare class.
- [ ] T022 Run `nix develop --command just test` and `cargo clippy --workspace --lib --tests -- -D warnings`; both must be clean before push.
- [ ] T023 Deploy, then verify BY QUERY per `specs/003-cmaf-hw-affinity/quickstart.md` §3: report the actual output of `sum by (device) (pharos_segment_produced_total)` and `sum by (outcome) (pharos_transcode_pin_total)`, against the measured baseline (420 cpu / 3 Nvenc, median 3380 ms vs 1825 ms).
- [ ] T024 Run the three-browser SyncPlay acceptance (quickstart §4): one title, three browsers, expect GPU encodes, no `outcome="shed"` on interactive jobs, and no `readiness gate timed out`. This is the scenario that failed on 2026-07-27.
- [ ] T025 Record the outcome in `specs/001-pharos-baseline/`: a B-entry if any defect was found in flight, and an invariant covering "a shared-init rendition is produced by exactly one encoder" so the class is guarded even if this implementation is later replaced.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (gate)** → blocks everything. T001 red ⇒ T002 and stop.
- **Phase 2 (foundational)** → blocks all user stories. T006/T007 in particular MUST precede Phase 5.
- **Phase 3 (US2)** → before Phase 5.
- **Phase 4 (US3)** → before Phase 5.
- **Phase 5 (US1)** → the enabling change; safe only after Phases 3 and 4.
- **Phase 6** → after Phase 5.

### User Story Dependencies — an honest deviation

The template's model is independent, individually shippable stories. That does
not hold here and pretending otherwise would be dangerous: **US1 is the feature,
but US2 and US3 are the safety properties that make US1 legal to ship.**
Delivering US1 alone reintroduces issue #114 — undecodable video, served with a
200, invisible to the server. So the order is US2 → US3 → US1, and US1's single
enabling line (T019) is deliberately last.

US2 and US3 are independently testable and independently valuable (they are
no-ops in production until T019), which preserves the intent of the model even
though the delivery order is fixed.

### Parallel Opportunities

- T004 and T005 (different files, after T003).
- T009 and T010 (same file, different tests — sequence if edit conflicts arise).
- T020 and T021 (independent additions).
- Phase 1 has NO parallelism: it is a single gating measurement.

## Implementation Strategy

**MVP = Phases 1 → 2 → 3 → 4 → 5.** There is no smaller safe increment: the
foundational cache change and both guards are prerequisites to the one-line
change that delivers the value.

**Stop conditions.** T001 red ⇒ abandon. T009 or T014 passing on first run
without having been seen red ⇒ the test is not exercising the path; fix the test
before trusting it.
