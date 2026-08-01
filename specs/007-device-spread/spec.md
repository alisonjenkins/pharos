# 007-device-spread — use every encoder that can legally take the work

**Status**: designed 2026-08-01
**Depends on**: 006 (the queue, the learned per-device allowance, `urgency_key`),
spec 003 R8 / V80 / #114 (the shared-init one-encoder rule), PR #70 (browser H264
is CMAF)

## The problem, measured

The deployment has twelve encode permits and the path that carries almost all of
its traffic can reach eight of them:

```
pharos_transcode_device_capacity{device="Nvenc:0"} 8
pharos_transcode_device_capacity{device="cpu"}     4
```

The cause is one line in `DeviceTable::rendition_device` (`device.rs:276`):

```rust
let pool = if hw.is_empty() { supporting } else { hw };
Some(pool[(rendition_key % pool.len() as u64) as usize])
```

If *any* hardware device supports the codec, the CPU is dropped from the pool
entirely. With one GPU the pool is `[Nvenc:0]` and **every** H264 fMP4 rendition
pins there. `place()` then narrows candidates to that single device and never
widens — deliberately, including under cooldown, because widening is the #114
hazard.

So today, per rendition class:

| work | eligible pool | permits reachable | the other device |
|---|---|---|---|
| **H264 fMP4 (CMAF)** — all browser playback | `[Nvenc:0]` | **8** | 4 CPU permits idle |
| **VP9 fMP4** | `[Cpu]` (no VP9 encoder on a GTX 1070) | **4** | 8 NVENC permits idle |
| **mpegts** — native / TV clients | any eligible, first-fit | 12 | — |

Two viewers on CMAF do not share twelve permits. They share eight, and 006's
queue — which now orders their work correctly — serialises it onto those eight
while four sit unused. The queue made the *ordering* right; it cannot make more
capacity appear.

**What the queued population actually is**, because it bounds every claim below:
exactly one production call site submits a queued segment job
(`hls_cache.rs:2276`, `submit_tracked`). Every other scheduler entry point is
`submit_live`, which takes a permit directly and never queues. And
`SegmentContainer` has two values, `Mpegts` and `Fmp4`. So the queue holds
segment jobs only, and they are pinned iff they are fMP4 with a real video
encode.

## What must remain true

The constraint from #114 is **not** "hardware only". It is:

> Every segment delivered under one `avcC` init comes from one encoder.

NVENC emits Main profile with `log2_max_frame_num` 8; libx264 emits High with 4.
No ffmpeg flag reconciles them, and it cannot be patched in the container either,
because that field is the *width* of `frame_num` in every slice header. A segment
from the wrong encoder is undecodable and is served with a 200.

`rendition_device` already enforces the real rule — one rendition resolves to
exactly one device — and it does so as a **pure function of the rendition**, so
the answer survives a restart. An in-memory pin would not, and a rendition
re-pinned mid-playback serves segments that no longer match the init a client is
still holding.

Nothing in this spec may weaken either property.

## Explicitly not achievable, and why it is worth stating

The obvious formulation — *"NVENC takes the time-critical segments, the CPU takes
the speculative ones"* — is **illegal within one viewer's stream**. A viewer's
prefetch and their live segments are the same rendition under the same init.
Splitting them across encoders is exactly the #114 bug.

Urgency-based *device* routing is therefore only expressible **between**
renditions, never between segments of one. What already exists for within-device
urgency, and needs nothing further:

- `urgency_key`'s first element is the class tier, and it is absolute — every
  Interactive job in the queue is dispatched before any Background one, so a
  device only picks up speculative work when no client is waiting on anything it
  could take.
- `crowds_a_client` caps how much speculation may run beside a client's segment
  *on that device*, at a value 006 learns per device from observed deadlines.

Both shipped 2026-07-31. This spec adds cross-device spreading, not
within-device priority.

## Phase A — put the CPU back in the rendition pool

**The change**: `rendition_device`'s pool becomes every supporting device,
weighted by capacity, instead of the hardware subset.

Correctness is unchanged and this is the whole argument: each rendition still
resolves to exactly one device, still by a pure function of the rendition, still
identically after a restart. What changes is only *which* device — and therefore
that two concurrent renditions can occupy two devices instead of queueing on one.

**Weighting matters.** A naive `rendition_key % 2` sends half of all renditions
to a CPU that has half the permits and is several times slower per segment. The
pool should be capacity-weighted — 8 NVENC slots to 4 CPU slots, i.e. two thirds
of renditions to NVENC — so the split reflects what each device can actually
carry. Weighting by *throughput* rather than permit count would be better still
and needs a measurement this spec does not have; permit count is the honest
first approximation and 006's per-device allowance will correct for the rest.

**What a CPU rendition costs.** Software x264 is slower per segment than NVENC.
That is a real regression *for that viewer* against being alone on the GPU, and a
real improvement against being queued behind another viewer. The trade is only
favourable under contention — which is precisely when the queue exists.

### The restart problem, which is the reason this is a spec and not a patch

Any placement that is not a pure function of the rendition re-pins differently
after a restart, and a client holding a pre-restart init then receives segments
from the other encoder. That is #114 with extra steps. Three ways out:

1. **Capacity-weighted hashing** — stays a pure function, so restart-safe by
   construction. Load-blind: an unlucky hash can put a lone viewer on the CPU
   while NVENC idles. *Recommended for phase A*, because it is the only option
   that changes no invariant.
2. **Persist the pin** — a `rendition → device` row, so placement can be
   load-aware at first sight and stable thereafter. Costs a migration, a write on
   the segment path, and an eviction policy for renditions nobody will watch
   again.
3. **Generation in the init URL** — a re-pin bumps a generation the client must
   re-fetch the init for. Most correct, most moving parts, and it changes a URL
   shape that jellyfin-web and every native client construct.

Option 1 is what phase A should ship. Options 2 and 3 are the door to load-aware
placement and should not be opened until the phase A signal says the hash is
actually mis-placing work.

### The signal that says whether it worked

Named before the code, per the ODD rule.

- `pharos_transcode_device_in_use{device}` already exists. The question this
  phase answers is whether both devices are ever non-zero *simultaneously* under
  two-viewer load. Today they cannot be for CMAF.
- **New**: `pharos_transcode_rendition_pin_total{device}` — how many distinct
  renditions resolved to each device. This is the weighting made visible; if the
  ratio is not near 2:1 on this hardware, the weighting is not doing what it
  claims.
- The failure mode to watch is a lone viewer landing on the CPU while NVENC is
  idle. It is visible as `device_in_use{device="cpu"} > 0` with
  `device_in_use{device="Nvenc:0"} == 0` and only one active session — a query,
  not an assumption, and the trigger for considering option 2 or 3.
- 006's `pharos_transcode_background_allowance{device="cpu"}` becomes meaningful
  for the first time on the CMAF path. It has been seeded at 1 and never
  exercised.

### Tests

- a rendition resolves to the same device across two `DeviceTable`
  constructions — the restart property, which is the one that must not break
- over many rendition keys, the device split matches the capacity ratio within a
  stated tolerance
- a rendition whose codec no hardware supports (VP9) still resolves to the CPU
- a rendition pinned to the CPU is *dispatched* to the CPU on both the arrival
  and the queue-drain path — the asymmetry 006 had to fix once already

## Phase B — class-preferred device ordering for unpinned work

Smaller than it first appears, and the spec should say so rather than flatter
itself: unpinned **queued** work is mpegts only, which on this deployment is the
native/TV path, not the browser path. Phase A is where the traffic is.

`eligible_for` returns devices in table order (hardware first, CPU appended) and
`place()` takes the first with a free permit. There is no class awareness. For
unpinned work it can be added cheaply: an Interactive job tries hardware first, a
Background job tries the CPU first, and each spills to the other when its
preference has no free permit.

That is the rule as originally asked for, and for unpinned work it is legal
segment-by-segment because there is no shared init to violate.

- **Do not** let a background preference become a background *restriction*. If
  the CPU is full and NVENC is free, speculative work still runs on NVENC — 006's
  `crowds_a_client` is what stops it hurting a client there, and it is already
  load-bearing.
- Signal: `pharos_transcode_decode_accel_total` already carries `{device,class}`.
  The question is whether the class/device correlation actually shifts; if it
  does not, phase B did nothing and should be reverted rather than kept for
  tidiness.

## Out of scope

- **Throughput-weighted placement.** Needs a per-device segments-per-second
  measurement that does not exist. Permit-count weighting first; measure; then
  decide.
- **Splitting one rendition across encoders.** Illegal, permanently — see above.
- **Re-pinning a live rendition to rebalance.** Same hazard.
- **A second GPU.** `rendition_device` already spreads across multiple hardware
  devices; nothing here is needed for that case.
- **Non-segment work** (images, trickplay, subtitles). It never reaches the
  transcode scheduler.

## Degradation

| situation | result |
|---|---|
| hash puts everything on one device | worst case is today's behaviour |
| CPU rendition under-performs | 006's per-device allowance throttles speculation there independently |
| no hardware present | pool is `[Cpu]`, identical to today |
| hardware in cooldown | pin still resolves to it and the job FAILS rather than spilling — unchanged, and deliberate (V80) |
| phase B mis-orders | spill means no job is ever refused a free permit |

## The measurement that justifies the whole thing

Before: two CMAF viewers, one device, `device_in_use{Nvenc:0}` saturated and
`device_in_use{cpu}` at zero, with the second viewer's segments queued.

After: both non-zero, and the second viewer's `queue_wait_seconds` materially
lower.

If that second measurement does not move, phase A did not help and should be
reverted — a spread that costs a viewer NVENC and buys nothing is worse than the
serialisation it replaced.
