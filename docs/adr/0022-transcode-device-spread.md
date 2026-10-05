# ADR-0022: Weighted spread of transcodes across CPU and GPU devices

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (designed and phase A implemented 2026-08-01; backfilled 2026-10-05)
- **Deciders:** Alison

> Backfilled retroactively from `specs/007-device-spread/` and the code; the
> date reflects when documented, not when decided.

## Context

ADR-0020 pins each shared-init fMP4 (CMAF) rendition to one encoder, because
every segment under one init must come from the same encoder (V80, issue
#114). The pin is `DeviceTable::rendition_device`, a pure function of the
rendition.

Its pool was hardware-only: if any hardware device supported the codec, every
software device was dropped. On a machine with an accelerator the CPU encoder
was therefore unreachable for every codec the accelerator could encode, however
many permits it had. The reverse also held: a codec the accelerator could not
encode (VP9 on the home box) was confined to the CPU while the accelerator
idled. The spec's worked example is the home deployment: one hardware encoder
at 8 permits, software at 4 (spec 007, "What this looks like on a real box").
006's queue fixed the order of work but cannot create capacity.

V126 constrains the fix: a performance tunable that must know the hardware is a
defect. Writing down a CPU-to-GPU ratio would be exactly that.

## Decision

Every device that supports the codec, software included, enters the pool of
`rendition_device`. Each device owns a band of the key space as wide as its
**weight**, and a rendition's device is the band its key lands in.

- **The weight is derived from the boot probe, never written down.** It is the
  device's permit capacity multiplied by a bucketed speed ratio (`RATE_BUCKET_RATIO`,
  doubling steps) against the slowest measured device. The rate comes from
  `probe_encode_rate`, which times every device on the same synthetic clip.
- **A ratio is legitimate where an absolute cost is not.** 006 found a boot-time
  trial encode a poor measure of real segment cost (decode and I/O dominate).
  The probe compares devices on identical input, so those costs are common-mode
  and cancel (spec 007, Phase A).
- **Placement stays pure.** It reads the rendition and the probe result, never
  load or cooldown. A device in cooldown still fails its pinned job rather than
  spilling to another (V80). Load management stays with 006's learned per-device
  allowance. This is the split the spec rests on.
- **Stability comes from persistence, not bucketing.** Rates and capacity are
  stored (`rate_store`, keyed by device-set identity) so unchanged hardware is
  not re-probed (V130). The HLS cache generation is composed from a digest of
  the `(device, weight)` pairs, so any change that could move a rendition also
  changes the `g=` in the init URL and invalidates cached segments (V129).
- **Phase B (class-ordered device preference for unpinned work) was built and
  dropped.** A weight measured on one codec (H264) cannot order work for
  another: it sent VP9 to the CPU, and `vp9_lands_on_vaapi_hardware` failed.
  `vp9_lands_on_vaapi_hardware` now guards against reintroducing it.

On the home box, `tasks.md` T111 records the probe as CPU 238 fps against
NVENC 192 fps per stream. Capacity 8 against 4 then gives NVENC two thirds of
the bands (B195). The spec itself records no measured ratio.

## Consequences

- Concurrent renditions can occupy several devices, and a machine whose CPU is
  the stronger device is weighted that way. Only a live-traffic query can show
  the benefit. The spec says to revert phase A if interactive queue wait does
  not move.
- Placement is load-blind. An unlucky key can put a lone session on a weaker
  device while a stronger one idles, and a weighted draw guarantees nothing
  per rendition. B195 hit this: a burn rendition drew the CPU third. V148 added
  a hardware preference keyed on burn intent, and B200/V153 fixed it to key on
  the rendition, not the per-segment flag. T111 keeps load-aware placement open.
- Changing the device set, the weights or the probe costs the segment cache and
  every client's cached init. That is intended and automatic (V129).
- Signals: `pharos_transcode_device_weight{device}` is the share given,
  `pharos_transcode_rendition_pin_total{device}` the share received (V131).
- A GPU-less machine behaves as before: a software-only pool.
- Not done: runtime re-placement of a live rendition, splitting a rendition
  across encoders (permanently illegal), quality-based device preference.

## Alternatives considered

- **Unweighted round-robin across devices:** rejected in spec 007. It sends half
  the renditions to the worse device.
- **Load-aware pinning:** excluded. `SegmentIdentity` has no device field, so a
  moved pin serves old and new encoders' segments under one init.
- **Class-ordered preference for unpinned work (phase B):** built, then dropped,
  as above. A codec-aware version needs a rate per codec per device, which the
  spec calls its own spec.

## References

- ADR-0020 (CMAF hardware affinity: the one-encoder pin this builds on)
- `specs/007-device-spread/spec.md` ("What shipped", Phase A, Phase B), `plan.md`
- `specs/003-cmaf-hw-affinity/` (shared-init one-encoder rule)
- Invariants V80, V126, V129, V130, V131, V148, V153; bugs B195, B200
  (`specs/001-pharos-baseline/`); `tasks.md` T111
- `crates/pharos-transcode/src/device.rs` (`rendition_device`, `device_weight`,
  `placement_fingerprint`), `rate_store.rs`, `probe.rs`;
  `crates/pharos-cache/src/hls_cache.rs` (`compose_generation`)
