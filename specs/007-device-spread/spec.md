# 007-device-spread — use every encoder the machine actually has

**Status**: designed 2026-08-01; **phase A implemented 2026-08-01, pending
deploy**; **phase B built, measured and dropped** (see its section).
**Depends on**: 006 (the queue, the learned per-device allowance, `urgency_key`,
and **V126**), spec 003 R8 / V80 / #114 (the shared-init one-encoder rule)

## What shipped, and where it diverged from this design

Phase A landed as designed. Three things the design did not anticipate, each
forced by something implementation found:

1. **Quantisation cannot deliver placement stability, so persistence does.** The
   design leaned on bucketing the measured rate coarsely enough that noise could
   not move a weight. That is not achievable: `.round()` *relocates* the
   zero-margin set from `{2^k}` to `{2^(k+0.5)}` rather than shrinking it, both
   sides of the ratio are noisy, the reference is a `min` over a set so one noisy
   device re-places renditions on every other, and a probe **timeout** shifts the
   reference discontinuously on unchanged hardware. The measured rates are now
   persisted (`rate_store`) keyed by device identity, so unchanged hardware
   reuses them and skips probing — placement is stable by construction rather
   than by probability, and boot is faster.
2. **The cache generation is derived from the placement rule.** `HLS_GEN_VERSION`
   alone closed only the deploy transition. Because weight is capacity in
   production and `probe_device_caps` under-reports on a loaded box — its own doc
   says so, and the chart runs it every pod start — a restart under load could
   re-place renditions with a warm cache and an unchanged `g=` in the client's
   immutable init. The generation is now
   `{HLS_GEN_VERSION}-{digest of the sorted (device_id, weight) list}`, read by
   both the server-side wipe and the client-visible `g=`. Any future change to
   the weighting, the probe or the device set invalidates automatically.
3. **A capacity is ratcheted only if a probe measured it.** `max(stored, probed)`
   is sound for `probe_device_caps` output, because it only ever reports a
   concurrency it demonstrated — so a drift down is an artefact. It is *not*
   sound for a computed CPU permit count or an operator's configured session cap,
   where a change is genuine; ratcheting those would latch a down-sized VM's old
   core count forever, silently, since a software encode is never refused.

**Phase B was implemented and then dropped**, which is recorded in full in its
own section: a device weight measured on one codec cannot order work for another,
and `vp9_lands_on_vaapi_hardware` caught it.

## The portability rule this spec is answerable to

**V126**: *a performance tunable that must know the hardware is a defect.* 006
existed to remove one. This spec must not introduce another, one level up.

pharos runs on whatever someone puts it on: a GPU-less NAS, a laptop iGPU, a
many-core server with no accelerator at all, one discrete GPU, several unequal
GPUs, or hardware with AV1 encode that today's deployment does not have. Every
number below is **derived from the probe at boot**. No device family is named in
a decision, no ratio is written down, and nothing in the design is true only of
the machine it was designed on.

The specific deployment appears in this document exactly once — as a worked
example in "What this looks like on a real box" — and nothing depends on it.

## The problem

`DeviceTable::rendition_device` (`device.rs:276`):

```rust
let pool = if hw.is_empty() { supporting } else { hw };
Some(pool[(rendition_key % pool.len() as u64) as usize])
```

If *any* hardware device supports the codec, every non-hardware device is dropped
from the pool. Each shared-init fMP4 rendition then resolves into that reduced
pool, and `place()` narrows candidates to the one device it names and never
widens.

The consequence is general, not specific: **on any machine with at least one
hardware encoder, the software encoder is unreachable for every codec the
hardware supports** — however many permits it has, and however fast it is.
Symmetrically, any codec the hardware cannot encode is confined to software while
the hardware idles. Concurrent renditions serialise onto a subset of the machine.

006 made the queue *order* work correctly. It cannot make capacity appear.

**What is actually in the queue**, because it bounds every claim here: exactly
one production call site submits a queued segment job (`hls_cache.rs:2276`,
`submit_tracked`); every other scheduler entry point is `submit_live`, which
takes a permit directly and never queues. `SegmentContainer` has two values,
`Mpegts` and `Fmp4`. So the queue holds segment jobs, and they are pinned iff
they are fMP4 with a real video encode.

## What must remain true

The #114 constraint is **not** "hardware only". It is:

> Every segment delivered under one init comes from one encoder.

Two encoders' parameter sets are not interchangeable — the deployment's measured
case is a hardware encoder emitting Main profile with `log2_max_frame_num` 8
where the software encoder emits High with 4 — and it cannot be reconciled by a
flag or patched in the container, because that field is the *width* of
`frame_num` in every slice header. The mismatch is a property of having two
encoders, not of which two.

`rendition_device` enforces the real rule — one rendition resolves to exactly one
device — as a **pure function of the rendition**.

### Why purity is load-bearing beyond restarts

`SegmentIdentity` (`hls_cache.rs:133`) keys the segment cache on media, segment
index, audio and subtitle selection, and bitrates. **It does not include the
device.** So if a rendition's device were ever to change, previously cached
segments from the old encoder would be served interleaved with fresh ones from
the new encoder, under one init — with no restart involved at all.

That makes the pure-function property stronger than "survives a restart". It is:
**a rendition's device may never change while any segment of it is cached or any
client holds its init.** Any design that re-places a live rendition must also
invalidate its cache *and* get the client to re-fetch its init. Nothing in this
spec does that.

## Explicitly not achievable

The natural formulation — *"the accelerator takes the time-critical segments, the
CPU takes the speculative ones"* — is **illegal within one viewer's stream**. A
viewer's prefetch and their live segments are the same rendition under the same
init; splitting them across encoders is #114.

Urgency-based *device* routing is therefore expressible only **between**
renditions. Within a device it already works and needs nothing:

- `urgency_key`'s first element is the class tier and it is absolute — every
  Interactive job is dispatched before any Background one, so a device picks up
  speculative work only when no client is waiting for anything it could take.
- `crowds_a_client` caps speculation running beside a client's segment on that
  device, at a value 006 learns per device from observed deadlines.

Both shipped 2026-07-31.

## Design — placement is probe-derived, load management is learned

This is the separation that makes the whole thing portable *and* safe:

| | derived from | changes at runtime? | why |
|---|---|---|---|
| **which device a rendition uses** | probe-time facts only | **no** | must not move while segments are cached or an init is live |
| **how much runs on that device** | 006's observed deadlines | yes, continuously | no correctness constraint; it is pure load management |

So adaptivity to unknown hardware comes from the **probe**, and adaptivity to
changing conditions comes from **006's controller**. Neither needs a constant,
and the pin stays pure.

### Phase A — every supporting device enters the pool, weighted by what it can do

Replace the hardware-only pool with **every device that supports the codec**,
each weighted by a probe-derived score.

Correctness is unchanged, and that is the entire argument: each rendition still
resolves to exactly one device, still by a pure function of the rendition, still
identically for a given probe result. Only *which* device changes — and therefore
concurrent renditions can occupy several devices instead of queueing on one.

**The weight must be derived, never written down.** Two probe-time inputs, both
of which pharos already collects or can collect on the same boot pass:

1. **Permit count** — `DeviceSlot::capacity`, already probed per device
   (hardware session budget by trial; software by
   `available_parallelism() / sw_encode_threads()`).
2. **Relative encode rate** — how fast each device encodes the *same* synthetic
   clip at boot.

On (2), and this is a deliberate departure from 006's finding rather than a
contradiction of it: 006 established that a boot-time trial encode is a bad
measure of *absolute segment cost*, because real cost is decode- and I/O-bound.
That is true and unchanged. But the question here is different — a **ratio
between devices on identical input**, where decode and I/O are common-mode and
cancel. The ratio is a defensible measure of encoder speed even though the
absolute number is not. Say so where the probe lives, or the next reader will
think it contradicts V126's sibling reasoning.

If (2) is unavailable or fails, permit count alone is the fallback, and the
fallback must be *stated in the code*, not implied.

**Why weighting rather than round-robin.** A device with one slow permit and a
device with sixteen fast ones are not interchangeable, and an unweighted split
sends half the renditions to whichever is worse. The weight is what makes the
design behave correctly on hardware nobody has tested it on — including the case
where the *software* encoder is the stronger device, which is real on a
many-core server with a weak or old accelerator.

**Purity across restarts.** The weight is a function of the probe result. On
unchanged hardware the probe returns the same answer and every rendition
re-resolves to the same device. If the hardware *does* change, renditions
re-place — which is correct, because the device set they were placed into no
longer exists.

There is one honest gap: a boot probe that returns a slightly different measured
rate on the same hardware could shift a weight enough to move a rendition, and
cached segments for that rendition would then be from the wrong encoder. **The
probe-derived score must therefore be quantised** — bucketed coarsely enough that
ordinary measurement noise cannot change it — and that quantisation is part of
the contract, not an implementation detail. A test must assert that two probe
runs on the same machine produce identical placement.

### Phase B — class-preferred ordering for unpinned work

Smaller than it looks, and the spec should say so rather than flatter itself:
unpinned **queued** work is mpegts only. Phase A is where the traffic is.

`eligible_for` returns devices in table order and `place()` takes the first with a
free permit; there is no class awareness. For unpinned work it can be added: an
Interactive job tries accelerators first, a Background job tries the least
contended device first, each spilling when its preference has no free permit.

- A preference must never become a **restriction**. If a background job's
  preferred device is full and another is free, it still runs — `crowds_a_client`
  is what protects a client there, and it is already load-bearing.
- "Accelerator first" for interactive work is itself a hardware assumption. On a
  box where the probe says software is faster, the preference should follow the
  probe, not the device class. Derive it; do not hardcode `is_hw()`.

**Status: implemented, then reverted — this is not the "phase A did nothing"
case, it is worse.** Phase B was built as Task 5:
`DeviceTable::eligible_for_class` (weight-ordered, descending for an
Interactive job, ascending for Background), wired into `candidates_for`'s
`full_eligible` for the unpinned path. It passed its own TDD test
(`preference_follows_the_probe_not_the_device_class`, proving the order comes
from the probe and never from `is_hw()`) and the phase A standing guard
(`speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for`) kept
passing. But it broke `scheduler::tests::vp9_lands_on_vaapi_hardware`, and
that test was right to fail.

The weight `eligible_for_class` would have to order by is measured on **one
codec** — the boot probe's synthetic clip is H264 — because that is the only
rate `crate::probe` collects. A single per-device number taken on H264 cannot
express a *codec-dependent* relative speed. VP9 is the concrete, provable
case: `capability::RelCost` already records that software VP9 (libvpx) is
`Expensive` — dramatically slower than a hardware VP9 encoder — while software
H264 is merely `Moderate`. So a device whose H264-measured weight ties or
beats a VAAPI accelerator's (an ordinary fixture, not a contrived one —
`table()` in `scheduler.rs`'s tests has CPU's permit-derived weight equal to
or above VAAPI's) is "the strongest device" for an H264 job and *also* "the
strongest device" for a VP9 job under this ordering, even though VAAPI is the
only sane place to encode VP9 on that machine. Because a preference is never a
restriction, that CPU placement is not a rare tail case: it is what any
Interactive VP9 job hits whenever the CPU's permit is free — every time, on
that fixture.

So the honest ledger is not "phase B measured as neutral." It is: the benefit
is mpegts-only (the smaller half, as stated above), and the cost is a genuine
placement regression for any codec whose software/hardware speed ratio
differs from H264's — VP9 provably does, and an existing test caught it on
the first run. Making the ordering codec-aware would need a per-device rate
measured **per codec**, not once on H264, which multiplies the boot probe's
cost (one trial encode per device × per codec instead of per device) and is
its own spec, not a follow-up inside this one.

`eligible_for_class` was removed. `candidates_for` and `place()` read
`eligible_for` for the unpinned path exactly as before phase B, unmodified.
`vp9_lands_on_vaapi_hardware` is the guard against re-introducing this: its
doc comment now says so, so the next attempt at phase B meets the reason
before it re-derives it.

## What this looks like on a real box

The single worked example in this document. Nothing depends on it.

The current deployment probes one hardware encoder at 8 permits and software at 4
(`available_parallelism()` 16 / `sw_encode_threads()` 4). Today its browser
renditions all resolve to the accelerator and the 4 software permits are
unreachable; its VP9 renditions are the mirror image, confined to software while
the accelerator idles, because that accelerator has no VP9 encode. Phase A puts
both on the board.

A GPU-less machine probes hardware to nothing, the pool is software-only, and
behaviour is byte-identical to today. A machine with two unequal accelerators
weights them apart instead of treating them as interchangeable — which the
current code cannot do either, since it round-robins the hardware pool by a bare
modulo.

## Signals

Named before the code, per the ODD rule. All are per device, from the probed
device set — no series name, label, or threshold assumes a device family.

- `pharos_transcode_device_in_use{device}` — exists. The question phase A answers
  is whether more than one device is ever non-zero *simultaneously* under
  concurrent load. Today, for any codec the accelerator supports, it cannot be.
- **New** `pharos_transcode_rendition_pin_total{device}` — distinct renditions
  resolved to each device. The weighting made visible. It should track the
  probe-derived weights; if it does not, the weighting is not doing what it
  claims.
- **New** `pharos_transcode_device_weight{device}` — gauge, the probe-derived
  score, published at boot. Without it, a misplacement cannot be told from a
  mis-weighting, and on unfamiliar hardware that is the first question.
- 006's `pharos_transcode_background_allowance{device}` becomes meaningful on
  devices that previously never saw this class of work.
- The failure mode to watch: a single active session pinned to a weaker device
  while a stronger one idles. Visible as one device in use, another at zero, and
  one session. It is a query, not an assumption — and it is the trigger for
  considering load-aware placement, which needs the cache-key and init-refetch
  work this spec deliberately excludes.

## Testing

- **Placement is stable**: two `DeviceTable`s built from the same probe result
  resolve every rendition identically. This is the invariant that must not break.
- **Placement is quantisation-stable**: two probe runs whose measured rates differ
  by ordinary noise produce identical placement.
- **The split follows the weights** across many rendition keys, within a stated
  tolerance — asserted against the *derived* weights, never against a literal
  ratio.
- **Codec capability is respected**: a rendition whose codec no accelerator
  supports resolves to software, on a table where an accelerator exists.
- **Degenerate tables**: no accelerator; one device; a device with one permit;
  two devices of equal score. None may panic, and none may produce an empty pool.
- **Both dispatch paths honour the pin** — arrival and queue-drain. 006 had to fix
  that asymmetry once already (it served undecodable video with a 200), and this
  spec widens which devices a pin can name.

## Out of scope

- **Runtime re-placement of a live rendition.** Needs the device in
  `SegmentIdentity` *and* an init the client will re-fetch. Both are real work and
  neither is justified until the phase A signal shows the probe-derived placement
  is actually mis-placing.
- **Splitting one rendition across encoders.** Illegal, permanently.
- **Quality-based device preference.** A software encoder at a slow preset can
  beat an accelerator per bit. Real, and a different spec.
- **Non-segment work** (images, trickplay, subtitles). It never reaches the
  transcode scheduler.

## Degradation

| situation | result |
|---|---|
| probe cannot measure encode rate | weights fall back to permit count, stated in code |
| all weight lands on one device | worst case is today's behaviour |
| a device under-performs its weight | 006's per-device allowance throttles it independently; placement is unaffected |
| no accelerator present | pool is software-only, identical to today |
| device in cooldown | the pin still names it and the job FAILS rather than spilling — unchanged and deliberate (V80) |
| hardware changed between boots | renditions re-place, which is correct — the old device set is gone |

## The measurement that justifies it

Before: two concurrent renditions of a codec the accelerator supports, one device
saturated, every other device at zero, the second rendition's segments queued.

After: more than one device in use, and the second rendition's
`queue_wait_seconds` materially lower.

If that second number does not move, phase A did not help and should be reverted.
A spread that costs a rendition the better device and buys nothing is worse than
the serialisation it replaced — and that verdict must be reached by query on the
machine in question, not assumed from the machine it was written on.
