# 006-self-tuning-playback — measure the hardware instead of guessing it

**Status**: designed 2026-07-31; **phase 1 shipped and validated in production
2026-07-31**; phases 2a and 2b implemented, pending deploy.
**Depends on**: B177/V125 (`background_alongside_client`, `peer_jobs`
instrumentation), B108/V58 (`background_headroom`, defer-and-rank)

## What shipped, and where it diverged from this design

Phase 1 landed as PR #188 and was **measured on the deployment under real
playback**, which is the only evidence that counts here:

| signal | observed |
|---|---|
| `pharos_transcode_background_allowance{device="Nvenc:0"}` | **2** (floor is 1) |
| `pharos_transcode_margin_total{verdict="met"}` | 1; no `missed`, no `ignored` |
| interactive `encode_seconds` | 1.292 s against a 3.0 s deadline |
| background | 22 encodes, mean 2.17 s |

Both halves of the gate hold: the allowance climbed off the floor **while**
interactive encode stayed well inside its deadline. The control law works on this
hardware.

Four divergences from the design above, each forced by something implementation
found that the design did not know:

1. **Promotion had to land BEFORE the queue, not after.** This document names
   promotion as the insight that makes a queue safe; the implementation plan
   sequenced it afterwards anyway. Without it, `register_or_join` coalesces an
   interactive request onto a queued *Background* driver whose class is fixed at
   registration, so the client waits at background tier behind every interactive
   job including later arrivals, with no timeout. A different mechanism from
   B108's lock, the same harm.
2. **The allowance gauge is seeded at boot**, not on first observation. Scraping
   the live pod found both controller series absent because the `metrics` crate
   registers lazily and nothing had been played since the pod started — making
   "not deployed", "deployed but idle" and "deployed but wedged" indistinguishable
   from a dashboard. `margin_total` is deliberately NOT seeded: a counter starting
   at zero would lie about traffic, while its absence correctly means no
   observation was ever made.
3. **Six queue outcomes, not four.** `dispatched` / `stale` / `evicted` / `shed`
   did not account for every exit; `abandoned` and `failed` were added so the arms
   partition jobs rather than approximately covering them. Counted once per job
   (`retries == 0`) so every arm shares one denominator.
4. **The shed/failure split needed a second value.** `coalesced_failed` initially
   counted an inherited `SchedulerBusy` as an encode failure, which under a
   prefetch storm would have made an alert fire on admission control working
   correctly. `coalesced_shed` keeps the two sums disjoint.
5. **The detached driver had to learn when to stop.** 2a's detachment made
   `PrefetchRegistry`'s abort a no-op against the encode itself, silently
   reversing PR #75 and falsifying a clause of V58. A driver now races its
   encode against "every requester has gone", decided under the registry's shard
   lock, and an encode stopped that way is counted `outcome="cancelled"`. The
   region it races over ends at DISPATCH: a job with a worker keeps its permit
   and finishes regardless, so abandoning it reclaims nothing and merely leaves
   an orphan writing to its successor's temp file. See the cancellation table in
   phase 2b.
6. **A promoted job is measured from the promotion, and labelled from it.**
   The controller judged every interactive completion from `dispatched`, which
   for a promoted job predates the client — a 5 s encode joined at 4 s reported
   5 s against a 3 s deadline and halved the allowance for a prefetch that
   landed. The observation window is now the later of dispatch and promotion,
   while `encode_ms` still measures the encode. Separately, the segment cache's
   latency histograms label by who was WAITING, as §Consequences requires;
   the production counters keep the driver's own class, since they count
   attempts rather than waits.
7. **Phase 2b has an off switch.** `SchedConfig::queue_background` (`[server]
   transcode_queue_background`) returns speculation to shed-not-queue without
   touching the interactive queue. `pending_cap = 0` is not the same lever: it
   sheds clients too.

Three defects this work found in already-shipped behaviour, all of which would
have been silent:

- An interactive request coalescing onto a background driver inherited its
  load-shed and reached the viewer as a **500 on a video segment** (B134 class,
  now V127).
- `try_place_no_queue` applied no shared-init fMP4 rendition pin, so a CMAF
  rendition pinned to NVENC that queued could be handed the CPU on drain —
  undecodable video served with a 200 (#114 / V80). Pre-existing, but this work
  routed the deployment's dominant fMP4 producer through that path.
- `place()` stamped peer counts at dispatch and the queue-drain path did not, so
  the instrumentation was blind on the only path that runs when a device is
  saturated (B178).

New invariants: **V126** (a tunable that must know the hardware is a defect),
**V127** (an outcome computed for one job's parameters must not be adopted as
another job's answer), **V128** (counter arms must share a denominator).
**V58**, **V80**, **V91** and **V125** were rewritten rather than left
contradicting the code.

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
beside a client's segment (phase 1), how in-flight work is shared rather than
locked (phase 2a), and which speculative work wins when there is not room for
all of it (phase 2b).

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

## Phase 2a — shared-result single-flight

Land after phase 1 is live. **Prerequisite for 2b**: it is the change that makes
a background queue safe rather than a repeat of B108.

### The lock that has to go first

`fetch_locks: HashMap<SegmentIdentity, Arc<Mutex<()>>>` is held for the **entire
multi-second transcode**. It is not guarding a data race — it is single-flight
dedup implemented as mutual exclusion. Holding exclusion across seconds of work
is precisely what makes a queued speculative job poisonous: a client asking for
the same segment blocks in `segment_fetch_lock_wait` for the whole queue wait,
which is the 90 s shape B108 removed the queue to escape.

Replace it with a **shared-result registry**: `DashMap<SegmentIdentity, _>`
holding a `watch` receiver (or equivalent shared future) per in-flight segment.

- The first requester inserts the entry and spawns a **detached driver** for the
  encode.
- Later requesters clone the receiver and await the **value**, not a lock.
- Nobody holds exclusion across work, so a queued job cannot block anyone.
- **Cancellation-safe**, which the current design is not: today, if the lock
  holder is cancelled mid-transcode the guard drops, the next waiter re-checks
  the filesystem, and the segment may be encoded twice. A detached driver
  outlives its original requester.
- Promotion (2b) falls out for free — the entry already carries the job id.
- The `post_lock` filesystem re-check disappears; the shared result *is* the
  answer.

### On "lock-free"

The queue itself is **already lock-free in the way that matters**: `SchedState`
is owned by a single task fed by an mpsc channel — an actor, no mutex. Priority
selection (2b) does not introduce one. Replacing it with a CAS-based concurrent
priority queue would add real complexity to remove contention that does not
exist, because only one task ever touches it. Deliberately not done.

`dashmap` uses sharded internal locks, so 2a is **not literally lock-free**
either. The win is "stop holding exclusion across long work" — nanoseconds of
map contention in place of seconds of held exclusion — not the elimination of
mutexes as such.

### Dashboard contract

`pharos_segment_cache_total{hit_path="post_lock"}` is a **dashboard contract**
and `post_lock` ceases to exist as a concept. The label needs a deliberate
migration to its successor (a request that coalesced onto an in-flight encode),
not a silent rename: V-class rule, a renamed label breaks alerts silently.

## Phase 2b — urgency-ordered background queue

Background work currently never queues; it is **shed and never retried**. With
two viewers the loser of the race has its prefetch dropped, so it takes a cold
interactive miss on every segment while the winner builds a deep buffer. Queuing
it means served late instead of never — which is the whole justification for
re-entering territory that has already caused one outage.

### The insight that makes a queue safe

**A client waiting on a speculative job's result is proof the speculation was
correct.** It is not speculation any more; it is demand. So do not make the
client wait behind it, and do not cancel and redo the work — **promote it**.

A client arriving on an in-flight entry (2a) reads the job id and calls
`scheduler.promote(job_id)`. The scheduler reclassifies that queued job
Background → Interactive and it jumps the tier. The client's wait collapses from
"queue + encode" to "encode", with no duplicated work.

### Structure

Keep `pending`; allow background in; **select at dispatch instead of popping the
front**:

1. **Tier**: every Interactive before any Background. Absolute.
2. **Within Interactive**: FIFO — all equally urgent, someone is blocked on each.
3. **Within Background**: ascending lookahead distance, **recomputed at dispatch**
   against the session's current playhead.

Point 3 is what makes it smart rather than merely ordered. Distance is a
property of *now*, not of the job: one queued 30 s ago at distance 6 may now be
distance 1 — the most urgent thing in the queue — or already passed. Freezing
urgency at submit time gets this exactly backwards. An O(n) scan with
`n ≤ pending_cap` (256) is cheaper than maintaining a heap whose keys all move
anyway.

`prefetch_target_segments` already loops `for ahead in 1..=ahead_count`, so the
distance exists at the submission site and is currently discarded.

### Cancellation

Evaluated at dispatch, so no reaper task is needed:

| condition | action |
|---|---|
| `reply.is_closed()` (session swap/stop via `PrefetchRegistry`) | drop — implemented, but see below: 2a broke it and it had to be re-made |
| background job whose distance ≤ 0 (playhead passed it) | drop as stale |
| queue full | evict the **least urgent background**, not the newest arrival |
| nothing evictable | shed, as today |

Eviction direction matters: FIFO overflow drops the newest, which is the most
urgent. Evicting the least urgent is the opposite, and correct.

The first row of that table is the one 2a quietly invalidated, and it is worth
stating plainly because it is the shape of the mistake: two individually
correct changes whose seam is wrong. `PrefetchRegistry` cancels by aborting the
spawned prefetch task. With a caller-driven fill that dropped the scheduler
`submit()` future with it. With a **detached driver** it drops only the
*waiter* — the `oneshot` lives on inside the driver, which nothing holds a
handle to — so `reply.is_closed()` is false for every abandoned prefetch and
the abandonment sweep collects nothing.

The fix is not to re-attach the driver. It is to make the driver outlive the
requester that STARTED it and not the last requester WANTING it: the registry
holds the publish channel's sender and no receiver, so `receiver_count() == 0`
means nobody is waiting, and the driver races the encode against that condition.
The abandonment is decided under the map's shard lock so a joiner arriving in
that instant either attaches (and the encode continues) or misses the entry
entirely (and drives a fresh one). An encode stopped this way is counted
`pharos_segment_produced_total{outcome="cancelled"}` — which is, for the first
time, the query that says how much speculative work an episode swap ABANDONS.
Not how much capacity it returns: the cancellable region ends at dispatch
(`JobSlot::is_dispatched`), because a job already handed to a worker keeps its
permit and runs to completion whatever the caller does, so only the QUEUED share
of that count was ever reclaimable. Ending the region there costs nothing —
those jobs were never going to be given back — and it stops an orphaned worker
writing to the key-derived `{seg}.ts.tmp` its own successor is about to use.

### What the scheduler must learn

It currently has no idea where a session's playhead is. Background jobs carry
`(session, segment)`; an Interactive submit updates
`session → last_requested_segment`; distance is `segment − last_requested`. The
map is bounded and LRU'd at 256, mirroring `MAX_TRACKED_SESSIONS` in
`PrefetchRegistry`.

### Consequences

- **`class` becomes mutable mid-flight.** A promoted job must report
  `class="interactive"` on `encode_seconds` — it ended up being demand, and
  labelling it background would understate interactive latency in exactly the
  case that matters.
- **Per-session fairness is still not guaranteed.** If A queues four jobs before
  B submits anything, A's still sort ahead at equal distance. A per-session
  background cap would close it; hold until the metric shows it is needed.

### The test this ships or dies on

A test reproducing B108's shape directly: a client requests a segment that is
already queued as background behind a saturated device, and the client's wait
must be bounded by **one encode**, not by the queue. If that cannot be made to
pass, the queue does not ship. The fallback is the design this replaced:
urgency-gated admission with no queue — `admit a background job at lookahead d
iff free_background_slots ≥ d`, so distance-1 work needs one free slot and
distance-6 needs six. That approximates the ordering without ever queuing, at
the cost of still dropping the loser's work rather than deferring it.

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
- Phase 2a: `pharos_segment_cache_total{hit_path}` gains the successor to
  `post_lock` — a request that coalesced onto an in-flight encode. Migrated
  deliberately, not renamed silently.
- Phase 2b: `pharos_transcode_queue_outcome_total{class,outcome}` —
  `dispatched` / `stale` / `evicted` / `shed`. The `stale` and `evicted` arms
  are the ones that say the queue is doing its job rather than merely
  accumulating; a queue that never evicts and never drops stale work is a queue
  that has quietly become the FIFO B108 deleted.
- Phase 2b: `pharos_transcode_promotion_total` — background jobs promoted to
  interactive because a client arrived on them. This is the safety valve made
  visible: promotions happening means speculation is being validated by demand;
  promotions at zero while clients wait means the promotion path is not wired
  up.
- Phase 2b: `pharos_transcode_queue_distance` — histogram of the lookahead
  distance of dispatched background jobs. Shallow-beats-deep becomes a query
  rather than an assumption.
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

Phase 2a:

- a second requester for an in-flight segment awaits the RESULT and never
  acquires exclusion — asserted by timing, not by inspection
- cancelling the first requester does not abort the encode, and the second
  requester still gets bytes (impossible today)

Phase 2b — **the test this ships or dies on**:

- a client requests a segment already queued as background behind a saturated
  device; its wait is bounded by one encode, not by the queue. This reproduces
  B108's shape directly. If it cannot be made to pass, the queue does not ship.
- a background job whose playhead has passed it is dropped at dispatch rather
  than encoded
- at queue capacity, the LEAST urgent background job is evicted and the newly
  arrived urgent one is kept
- with two sessions queued, every session's distance-1 job dispatches before any
  session's distance-6 job

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
