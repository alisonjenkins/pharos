# Implementation Plan: hardware encoding for CMAF renditions

**Branch**: `003-cmaf-hw-affinity` | **Date**: 2026-07-27 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `specs/003-cmaf-hw-affinity/spec.md`

## Summary

Replace the blanket "hardware is ineligible for H264 in fMP4" rule with a
per-rendition device **pin**: the first segment of a rendition chooses a device by
the existing placement policy, and every later segment of that rendition goes to
that device or queues for it — never to another. This keeps the one-encoder
guarantee issue #114 requires while letting browser playback reach the GPU, which
today it cannot at all.

## Technical Context

**Language/Version**: Rust (workspace toolchain, pinned in `rust-toolchain.toml`)
**Primary Dependencies**: `pharos-transcode` (scheduler, device table, workers),
`pharos-cache` (`HlsSegmentCache`), `pharos-server` (HLS handlers)
**Storage**: on-disk HLS segment cache — identity change required (R3)
**Testing**: `cargo nextest`; the synthetic device table already used by the
`device.rs` / `scheduler.rs` tests
**Target Platform**: Linux, k8s; deployment host is NVENC-only (GTX 1070), no VAAPI
**Project Type**: single Rust workspace
**Performance Goals**: ≥90% of browser CMAF segments on hardware; median encode
1825 ms vs today's 3380 ms per 6 s segment (SC-001/SC-002)
**Constraints**: a rendition's delivered segments must come from exactly one
encoder — including cache hits, and across process restarts
**Scale/Scope**: single household; NVENC's 8 sessions is not a live constraint here

**Resolved unknowns** (open in the spec, closed in [research.md](./research.md)):

- Wait vs new generation → **wait/queue**. A new generation is not reachable
  mid-stream: the VOD playlist carries a single `EXT-X-MAP` and clients do not
  reload it (R2).
- Pin lifetime across restarts → **not persisted**; an unknown pin is treated as a
  fresh rendition (R2, data-model).

**NEEDS CLARIFICATION — gating, not deferrable**: R4 — whether a hardware encoder
emits byte-identical parameter sets across independent encodes. Not measurable
without the GPU host. If it is false, this plan is void and the CPU-only rule is
correct as written.

## Constitution Check

| Principle | Assessment |
|---|---|
| I. Wire compatibility is the product | No wire change. Playlists, init and segment URIs are untouched; only which encoder produces the bytes changes. **PASS** |
| II. Group sync must beat Jellyfin's | This is a group-sync fix in substance — the 2026-07-27 wedge was buffering under CPU saturation. **PASS** |
| III. Test-first, then prove it by query | Contract tests 1–4 precede implementation; SC-001/002 are PromQL, named in quickstart §3. Test 5 (R4) is a prerequisite measurement run BEFORE code. **PASS** |
| IV. It never panics, never leaks, never lies | The degraded path is the risk: a lost device must FAIL loudly, not spill. Specified (FR-004), asserted (contract test 2). Silent spilling would serve undecodable video under a 200 — the exact shape of "lies". **PASS**, conditional on test 2 |
| V. Types over conventions, actors over locks | The pin lives in the scheduler actor's own state, reached only by message — no shared lock. The rendition key is DERIVED from `TranscodeOptions`, so a newly added option cannot silently leave the key stale (R1). **PASS** |

**Gate result**: PASS, with one condition — R4 must measure green before any code
lands. No violation requires justification in Complexity Tracking.

## Project Structure

### Documentation (this feature)

```
specs/003-cmaf-hw-affinity/
├── spec.md
├── plan.md              # this file
├── research.md          # R1-R6, incl. the two findings that changed the design
├── data-model.md        # RenditionKey, RenditionPin, cache identity
├── contracts/
│   └── device-affinity.md
├── quickstart.md        # validation, incl. the R4 gate
└── checklists/
    └── requirements.md
```

### Source Code (repository root)

```
crates/pharos-transcode/src/
├── device.rs            # drop the H264+Fmp4 exclusion; gate on shared-init fMP4
├── scheduler.rs         # pin map, placement rules, pin metrics
└── options.rs           # RenditionKey derived from TranscodeOptions

crates/pharos-cache/src/
└── hls_cache.rs         # device in segment identity; HLS_GEN_VERSION bump
```

No `pharos-server` changes expected: the handlers already pass `TranscodeOptions`
through `submit`, and the scheduler derives the key itself (contract).

## Implementation Order

1. **Measure R4.** If parameter sets differ across independent hardware encodes,
   stop and record the refutation in `research.md`. Everything depends on this.
2. **`RenditionKey`**, derived from `TranscodeOptions`, plus a test that an
   encode-affecting field changes the key.
3. **Cache identity** (R3) — device in the path, `HLS_GEN_VERSION` bumped. Ships
   **before** hardware becomes eligible, so no stale CPU-era segment can ever be
   served under a hardware init. This ordering is the whole safety argument.
4. **Pin map + placement rules** in the scheduler. Contract tests 1–4 written
   first, each verified to fail before its rule exists.
5. **Remove the `device_supports` exclusion**, scoped to shared-init fMP4 (R6).
6. **Metrics + log lines** (FR-005), asserted in test.
7. **Deploy, then verify by query** (quickstart §3) and by the three-browser
   SyncPlay run (§4).

## Risks

| Risk | Handling |
|---|---|
| Hardware not self-consistent across processes (R4) | Gates everything; measured first |
| Stale CPU-era cache served under a hw init | Cache identity change ships first (step 3) |
| Pinned device saturated while CPU idle | Accepted: queue, don't spill. Watch `queued_on_pin` |
| >8 concurrent pinned renditions on NVENC | Not reachable on this deployment; would need a "pin only with headroom" rule (R5) |
| Reintroducing #114 | Contract tests 1, 2 and 4 exist for this; test 2 matters most |

## Complexity Tracking

No constitution violations to justify.

Worth stating what this plan does **not** fix: a mixed Firefox/Chrome group still
runs two ladders (VP9 + h264-CMAF) for one title, and VP9 stays on CPU regardless —
NVENC has no VP9 encoder and this host has no VAAPI. Halving that split is
capability negotiation, not this feature.
