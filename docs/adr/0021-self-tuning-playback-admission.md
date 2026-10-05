# ADR-0021: Self-tuning (AIMD) playback admission

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (phase 1 live 2026-07-31; backfilled 2026-10-05)
- **Deciders:** Alison

Backfill: written after the fact from `specs/006-self-tuning-playback/`, the
invariants and the code. Where the spec records no reason, this ADR says so.

## Context

ADR-0005 mints every HLS segment on demand, and each client request also
launches speculative prefetches for the segments after it. Two defects in how
that work was admitted came out of production profiling (spec 006, "The
problem, measured"):

1. **A hand-tuned cap that had to know the hardware.** B177/V125 capped
   speculative jobs beside a client's segment at
   `background_alongside_client: 1`. One client request had launched itself plus
   six prefetches on one GPU, and the client's own segment went from 1 860 ms to
   6 358 ms. The cap of 1 was calibrated on a single GTX 1070 with a 23 Mbps
   1080p HEVC source. On a card with more NVENC engines it under-uses the
   hardware; on a weaker box it is still too many.
2. **Background work was shed, never queued.** B108/V58 removed the queue
   because a queued prefetch held the segment cache's per-key `fetch_locks`
   across the whole wait, so the client's own request for that segment inherited
   it (about 94 s in the incident). With two viewers the loser's prefetch was
   dropped rather than delayed, so it took a cold miss on every segment.

ADR-0017 solved the same class of problem for scan and trickplay I/O with a
fixed capacity (8 permits idle, 1 while streaming). This ADR covers the
playback transcode path only. It leaves the ADR-0017 gate as it is.

## Decision

We will learn the speculative-concurrency limit per device from whether clients'
segments made a deadline, and let background work queue safely behind it.

- **Phase 1, AIMD on the margin.** `pharos_transcode::admission` keeps an
  allowance per device, clamped to `[floor, ceiling]` (floor =
  `background_alongside_client`, ceiling = device capacity minus 1). The
  deadline is segment playback seconds times `margin_ratio` (0.5, so 3 s for a
  6 s segment). When an interactive job finishes: met and exercised, add
  `increase_step` (1.0). Missed with background peers present, multiply by
  `decrease_factor` (0.5). Otherwise no change. Admission floors the value.
  Live/progressive jobs and failed or retried jobs produce no signal.
- **Phase 2a, shared-result single-flight.** `fetch_locks` is replaced by a
  registry of in-flight encodes. Later requesters await the result, not a lock,
  and a detached driver outlives its first requester. It stops only when every
  requester has gone, and only before dispatch.
- **Phase 2b, urgency-ordered background queue.** Background jobs may queue.
  Dispatch selects Interactive before Background, FIFO within Interactive, and
  ascending lookahead distance within Background, recomputed at dispatch. A
  client arriving on a queued background job promotes it to Interactive instead
  of waiting behind it. Stale jobs are dropped, and overflow evicts the least
  urgent background job. `[server] transcode_queue_background` turns this off.
- **State is in memory and relearned each boot.**

The tuning constants are dimensionless (`margin_ratio`, `increase_step`,
`decrease_factor`). They describe how fast the loop reacts, not what the machine
can do (V126).

## Consequences

- No per-machine calibration. Phase 1 was measured live on 2026-07-31: allowance
  2 on `Nvenc:0` (floor 1), interactive encode 1.292 s against a 3.0 s deadline.
  Phases 2a and 2b were implemented but pending deploy when the spec was last
  updated; this ADR has not verified their production behaviour.
- Single-sample backoff is deliberate. The spec's reason: a miss means over half
  the segment's playback duration is gone, so reacting fast is the safe side.
- After a restart the first viewer gets today's behaviour (the floor) and the
  allowance climbs from there. Persisting it was rejected in the spec because a
  stale curve misleads after a hardware change.
- Per-session fairness is not guaranteed. Session A's four queued jobs sort
  ahead of B's at equal distance. The spec holds a per-session cap until a
  metric shows it is needed.
- New signals are dashboard contracts: `pharos_transcode_background_allowance`,
  `_margin_total{verdict}`, `_queue_outcome_total{class,outcome}` and
  `_promotion_total`. Arms must share a denominator (V128). A request must not
  inherit another job's load-shed (V127).
- The queue ships only because the "client waits one encode, not the queue"
  test passes. The spec's stated fallback was urgency-gated admission with no
  queue.

## Alternatives considered

- **Per-peer-count EWMA table.** It cannot learn costs at concurrency levels its
  own admission rule never permits, and fixing that needs forced exploration.
- **Fitted contention slope** (`encode = base * (1 + a * peers)`). It bakes in a
  shape and under-predicts near saturation, where the damage happens.
- **Persisting the learned model**, to Postgres or under a hardware fingerprint.
  Rejected for migration cost and stale-curve risk.
- **A lock-free CAS priority queue.** The scheduler state is owned by one actor
  task, so there is no contention to remove.

## References

- ADR-0005 (per-segment HLS transcode), ADR-0017 (background I/O gate, a
  separate resource)
- `specs/006-self-tuning-playback/spec.md` (control law, phases 2a/2b, signals),
  `plan.md`
- `specs/001-pharos-baseline/invariants.md` V58, V125, V126, V127, V128;
  `bugs.md` B108, B177, B178, B134
- `crates/pharos-transcode/src/admission.rs`, `scheduler.rs` (`promote`,
  `queue_background`), `crates/pharos-cache/src/hls_cache.rs`
