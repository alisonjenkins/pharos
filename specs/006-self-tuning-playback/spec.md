# 006-self-tuning-playback — measure the hardware instead of guessing it

**Status**: designed 2026-07-31
**Depends on**: B177/V125 (`background_alongside_client`, `peer_jobs`
instrumentation), B108/V58 (`background_headroom`, shed-not-queue)

## The problem, measured

B177 fixed a real defect — one client segment request launched itself plus six
speculative prefetches, all admitted to the one GPU at once, and the client's
own segment slowed from **1 860 ms to 6 358 ms** as a result. The fix caps how
much speculative work may run beside a client's segment.

The cap is `background_alongside_client: 1`, and that number is a **guess
calibrated on one machine**: a GTX 1070, transcoding a 23 Mbps 1080p HEVC
source. On a card with more NVENC engines it under-uses the hardware; on a
weaker box it is still too many. Nothing about it is derived from the machine it
runs on.

pharos already does a little hardware calibration: `hw_encode_session_budget` is
trial-probed at boot and the prefetch depth scales off it. The gap is that the
probe measures **capacity** — how many sessions the encoder will accept — and
not **throughput under contention**, which is what actually decides whether a
viewer waits. The live profile showed why that distinction matters: cost is
**decode- and I/O-bound, not encode-bound**. The same segment costs the same to
produce at 360p as at 1080p, for a sixteenth of the output:

| rung | output | cold cost |
|---|---|---|
| 1080p | 15 Mbps | 7.7 s |
| 720p | 3.4 Mbps | 7.7 s |
| 480p | 1.8 Mbps | 8.5 s |
| 360p | 0.9 Mbps | 8.7 s |

A boot-time trial encode of a synthetic clip measures precisely the component
that is not the bottleneck.

The second defect is ordering. Background work is **shed, never queued** (B108:
a queued speculative job holds the cache's per-key fetch lock, so the client's
own later request inherits the whole wait). Submission carries no notion of
*when* a segment will be needed — `JobClass` is binary, and the scheduler cannot
tell `seg+1` (wanted in 6 s) from `seg+6` (wanted in 36 s). With two viewers:

```
viewer A requests seg 100  → submits prefetch 101..106
viewer B requests seg 200  → submits prefetch 201..206
                             12 jobs race for the same slots, first-come-first-served
```

Whoever submits first takes the capacity and the rest are **dropped, not
delayed**. One viewer gets a deep buffer, the other gets none and takes a cold
interactive miss on every segment. B177 sharpens this by leaving fewer
background slots to win.

## Scope

**In**: the playback path only — how much speculative transcode work may run
beside a client's segment (phase 1), and which speculative work wins when there
is not room for all of it (phase 2).

**Out, deliberately**:

- **Prefetch depth.** It already scales on the probed GPU budget. Depth and
  concurrency interact — deeper prefetch at a higher allowance compounds — so
  changing both at once makes any result unattributable. Second iteration.
- **Non-playback constants.** Scan probe concurrency, trickplay
  `SWEEP_CONCURRENCY`, the `BG_IO_MAX` I/O gate, backfill pacing. Same class of
  problem, different resource; do them once the mechanism is proven.
- **Policy constants.** MusicBrainz's 1 100 ms rate limit, session TTLs, DB
  retention, `DECODE_PREROLL_SECONDS`. These are not hardware-sensitive.
  MusicBrainz's rate limit does not get faster on a better GPU.
- **Persisting the learned model.** In-memory, relearned each boot. See
  "Restart behaviour".

## What the tuner holds true

Not "N jobs". The invariant is a **realtime margin on the segment a client is
blocked on**: a segment covering 6 s of playback must be produced within a
fraction of its own playback duration. That target is hardware-independent and
semantically meaningful — the same rule admits many concurrent jobs on a fast
card and none on a weak one, with no constant to calibrate per machine.

`margin_ratio` defaults to **0.5**: a 6 s segment must be produced in ≤ 3 s.

## Phase 1 — AIMD on the margin

### Why a closed loop rather than a model

Two modelling alternatives were considered and rejected:

- **Per-peer-count EWMA table** (one bucket per concurrency level per device,
  admit if the estimate at `peers+1` clears the deadline). Predicts rather than
  reacts, but needs samples at every level and the admission rule itself
  prevents reaching the high ones — it can never learn what 4 concurrent jobs
  cost if it never permits 4. Fixable with forced exploration, at which point it
  is more machinery than AIMD for the same answer.
- **Fitted contention slope** (`encode ≈ base × (1 + α·peers)`; the deployment's
  curve gives α ≈ 0.39). Extrapolates from few samples, so no exploration
  problem, but it bakes in a shape and under-predicts near saturation — exactly
  where the damage occurs.

Both model *peer count*, a proxy. The measured cost is dominated by decode and
I/O, which vary per title and with NFS weather, so a model fitted on peers alone
predicts well for its training mix and mispredicts when the mix changes. A
closed loop measures the thing that matters — *did the client's segment make its
deadline* — so a slow NFS morning, a heavier codec, and another tenant on the
GPU all arrive as the same signal without being modelled separately.

### The control law

Per device:

```
state:    allowance: f64, clamped to [floor, ceiling]
floor:    background_alongside_client   (1 — never starve prefetch to zero)
ceiling:  device capacity − 1           (never fill a device with speculation)

on an interactive job finishing:
    deadline = segment playback seconds × margin_ratio
    met      = encode_seconds ≤ deadline

    met  AND background_peers ≥ allowance  →  allowance += increase_step
    miss AND background_peers > 0          →  allowance ×= decrease_factor
    otherwise                              →  no change
```

`increase_step` = 1.0, `decrease_factor` = 0.5.

The allowance is fractional so the multiplicative decrease is meaningful at
small values (2 → 1.5 → 1 rather than 2 → 1 → 1). **Admission floors it**: an
allowance of 2.5 permits two concurrent background jobs beside a client's
segment, not three. So a decrease takes effect immediately while the recovery
from 1.5 back to 2 costs one more successful observation — asymmetric in the
conservative direction, which is intended.

Both guards are load-bearing:

- **Increase only on an *exercised* observation.** A segment that met its
  deadline while running alone is no evidence the allowance can safely rise — it
  never tested it. Without this the allowance inflates on irrelevant successes
  and then discovers the truth by hurting someone.
- **Decrease only when background peers existed.** A 4K HEVC source that blows
  the deadline *alone* is not prefetch's fault and backing off cannot help.
  Without this one pathological title ratchets the allowance to the floor
  permanently and the tuner looks broken.

Producing no signal at all: live/progressive jobs (no `duration_ticks`), and any
job that failed or retried (its encode time means nothing).

Worked example — 6 s segments, deadline 3 s, capacity 8, ceiling 7:

| observation | bg peers | encode | verdict | allowance |
|---|---|---|---|---|
| start | | | | 1 |
| ran with 1 bg | 1 | 1.9 s | met, exercised | 2 |
| ran with 2 bg | 2 | 2.4 s | met, exercised | 3 |
| ran with 3 bg | 3 | 3.4 s | **missed** | 1.5 |
| ran with 1 bg | 1 | 2.0 s | met, exercised | 2.5 |

Settles around 2–3 on this hardware, and somewhere else entirely on a card with
a flatter contention curve. That is the point.

**Single-sample backoff is deliberate.** A miss means the segment took over half
its own playback duration, with ~3 s of slack still in hand. That is a strong
signal and reacting fast is the safe direction; waiting for a second sample
spends a viewer's buffer to avoid a false positive that costs almost nothing.

### Where it lives

A new module, `crates/pharos-transcode/src/admission.rs`. The controller is a
pure state machine over (observation → allowance), so it is testable without
spawning a scheduler. `scheduler.rs` is already 2 261 lines carrying placement,
retry, queueing, live streams and metrics; this does not go in it.

The scheduler actor already has everything the controller needs: `JobFinished`
carries the encode duration, `JobCtx` carries the class, the device and
`opts.duration_ticks`. No new cross-crate plumbing.

One addition to what B177 shipped: `peer_jobs` counts *all* peers. The
controller needs the **background** peer count specifically, recorded at
dispatch alongside it.

## Phase 2 — urgency-gated admission

Land after phase 1 is live, for the same reason prefetch depth is out of scope:
the allowance is an input to the urgency gate, and shipping both at once makes a
regression unattributable.

`prefetch_target_segments` already loops `for ahead in 1..=ahead_count`, so the
lookahead distance exists at the submission site and is currently discarded.
Carry it, and grade admission by it:

```
admit a background job at lookahead d  iff  free_background_slots ≥ d
```

where `free_background_slots` is the learned allowance minus background
in flight when a client job is on the device, and the device capacity minus
background in flight when it is idle.

Distance-1 work needs one free slot and so gets in almost always; distance-6
work needs six and runs only when the device is genuinely quiet. Under
contention **every session's next segment beats any session's sixth**, which is
the desired ordering, without reintroducing the queue B108 removed.

Deep prefetch still runs in the gaps between client requests, which is when
buffer-building should happen.

Two honest limits:

- **This approximates deadline ordering; it is not strict.** Within a single
  submission burst, admission among jobs that clear the gate is still
  submit-order. Strict ordering needs a queue, and the queue is what produced
  the 90 s waits B108 fixed.
- **It does not guarantee per-session fairness.** If A clears the gate on four
  jobs before B submits anything, A still gets more. A per-session background
  cap would close that; hold it until the metric shows it is needed.

## Restart behaviour

In-memory, relearned each boot. Rejected: persisting to Postgres (costs a
migration and a write path, and a stale curve actively misleads the admission
rule after a hardware change) and persisting under a hardware fingerprint (most
correct, most moving parts).

The deployment ran **92 pod instances in 30 days**, so this matters. Each
produced segment is one sample, so the loop converges within a single viewing
session. After a rollout the first viewer gets exactly today's behaviour — the
floor — and it climbs from there. No regression, just no head start.

## Degradation

Every failure path lands on today's behaviour rather than something new:

| situation | result |
|---|---|
| cold start, no observations | allowance = floor = exactly today |
| no interactive traffic | no signal, allowance frozen — harmless |
| only live/progressive streams | all observations ignored, stays at floor |
| pathological source missing deadline alone | guarded, no backoff |
| measurement noise | bounded by `[floor, ceiling]` |
| CPU spill | state is per-device; a slow CPU encode never pollutes the GPU |

Decrease is fast and increase slow, so the loop errs conservative. Worst-case
oscillation is a flap between floor and floor+1 — the range already shipped.

## Signals

Named before the code, per the ODD rule.

- `pharos_transcode_background_allowance{device}` — gauge, the learned value.
  This *is* the tuner: if it never leaves 1, the tuner is not working.
- `pharos_transcode_margin_total{device,verdict}` — `met` / `missed` /
  `ignored`. The `ignored` arm is load-bearing: a frozen allowance with
  all-ignored observations means *no signal*, which is a completely different
  problem from a device that genuinely cannot go faster.
- Phase 2: `pharos_transcode_prefetch_admit_total{verdict,lookahead_bucket}` —
  `admitted` / `shed`, bucketed by distance, so "shallow prefetch is winning" is
  a query rather than an assumption.
- Log only when the **integer** allowance changes, carrying observed, deadline
  and background peers. Not per observation — that would be noise at segment
  rate.

The query that proves phase 1 landed:

```promql
pharos_transcode_background_allowance{device="Nvenc:0"}
```

climbing above 1 under playback, **while** interactive `encode_seconds` p95
stays under the deadline. Both, not either: a rising allowance beside a rising
p95 means the control law is wrong.

## Testing

The controller is a pure state machine, so most tests need no scheduler:

- a fast device climbs to the ceiling and stops there
- a slow device collapses to the floor and stays
- an unexercised success does not increase
- a solo miss does not decrease
- clamping holds at both ends
- ignored job kinds change nothing

Scheduler-level:

- a cold controller behaves exactly like today — **the existing
  `speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for` test
  must pass unchanged**; that is the regression guard
- a fast scripted device lets progressively more background through
- phase 2: under contention, a distance-1 job is admitted where a distance-6 job
  from the same burst is shed

Every signal disarm-verified: the metric assertion must go red with the
instrumentation removed.

## The constants this replaces, and the ones it adds

Replaced: `background_alongside_client`, a number that had to know about the
GPU.

Added: `margin_ratio` (0.5), `increase_step` (1.0), `decrease_factor` (0.5).
These are dimensionless and hardware-independent — they describe how fast the
loop reacts, not what the machine can do — so they do not need re-picking per
machine. That is the whole trade: constants that must know the hardware are
replaced by constants that must not.
