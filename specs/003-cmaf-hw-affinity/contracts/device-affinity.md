# Contract: rendition → device affinity

Internal contract between `pharos-server`'s HLS handlers and
`pharos-transcode`'s scheduler. No wire/HTTP surface changes.

## Scheduler

```
submit(input, opts, sink, class)      // unchanged signature
```

The scheduler derives the `RenditionKey` from `opts` itself. Callers do not pass a
device, and cannot: letting a caller name one would make the guarantee a
convention rather than an invariant.

### Placement rules

| Condition | Behaviour |
|---|---|
| shared-init fMP4, key unpinned | place by existing policy (hw first, least-loaded); record the pin |
| shared-init fMP4, key pinned, permit free | dispatch to pinned device |
| shared-init fMP4, key pinned, permit busy | QUEUE on the pinned device — never spill |
| shared-init fMP4, pinned device in cooldown | invalidate pin, return `SchedError::Failed` |
| not shared-init fMP4 (mpegts, etc.) | unchanged free load-balancing (FR-006) |

`device_supports` no longer returns `false` for H264+Fmp4 (that exclusion is what
this replaces). The one-encoder guarantee moves from "no hardware is eligible" to
"one device is chosen and adhered to".

## Observability (FR-005)

Existing `pharos_segment_produced_total` and the `transcode_job` span's `device`
field already carry the device. Added:

- `pharos_transcode_pin_total{outcome}` — `pinned` (first placement),
  `followed` (dispatched to an existing pin), `queued_on_pin` (waited rather
  than spilled), `invalidated` (device lost).
- A log line on pin and on invalidation carrying the rendition key hash, the
  device, and the reason. The reason names the offending value, never a bare
  class.

`queued_on_pin` is the one to watch: it is the cost of the guarantee, and a rising
rate means the GPU is undersized for concurrent renditions (R5).

## Test contract

These are the assertions that make the guarantee real, not documentation:

1. **Never mixes** — segments for one key, dispatched while the pinned device is
   saturated and CPU is free, all report the pinned device. Fails if any spills.
2. **Cooldown does not spill** — inducing cooldown on the pinned device yields an
   error, NOT a CPU dispatch.
3. **mpegts unaffected** — an mpegts H264 job still load-balances across devices.
4. **Cache cannot cross pins** — a segment cached under device A is not served for
   a rendition pinned to device B.
5. **Hardware self-consistency** (R4, gating): two independent encodes on one
   device emit byte-identical SPS/PPS, including across a worker restart.

Test 5 is a prerequisite, not a regression test — if it fails, the feature does
not ship and `device_supports` keeps its exclusion.
