# ADR-0020: CMAF HLS renditions with per-device hardware affinity

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (documented; spec 003 written 2026-07-27, first live 2026-07-28)
- **Deciders:** Alison

> Backfill: written retroactively from `specs/003-cmaf-hw-affinity/` and the
> code as of 2026-10-05.

## Context

ADR-0005 chose independent per-segment transcodes. For shared-init fMP4 (CMAF)
that has a hard constraint: every segment is decoded under one `avcC` carrying
segment 0's parameter sets, and libx264 and the hardware H264 encoders emit
incompatible SPS. A segment produced by a different encoder than seg0 is
undecodable (issue #114). `device_supports` therefore made hardware ineligible
for H264 in fMP4 (spec.md, Problem).

That rule became expensive once PR #70 moved all browser H264 to CMAF. On an
NVENC-only host every browser path ran on CPU. Measured 2026-07-27: 420 of 423
jobs on CPU at 1.81x realtime while NVENC sat idle. Three browser viewers
exhausted the 4 CPU permits, members buffered, and the SyncPlay readiness gate
froze the group.

The real requirement is "every segment of a rendition comes from one encoder",
not "CPU only".

## Decision

A shared-init fMP4 rendition resolves to **exactly one device, as a pure
function of the rendition**, and the scheduler never places it anywhere else.

- **The device is computed, not remembered.** `DeviceTable::rendition_device`
  (`crates/pharos-transcode/src/device.rs`) picks from the devices that support
  the encode, by weighted bands over `RenditionKey % total_weight`. An
  in-memory pin map was the first design (spec.md, plan.md). It was replaced
  because a pin dies on restart while the disk cache and the client's init
  survive (research R8).
- **The key is derived from `TranscodeOptions`** with the time fields zeroed
  (`options.rs`, research R1), so a newly added encode option cannot leave it
  stale.
- **Cooldown and load never change the answer.** A busy device queues the job.
  A device in cooldown fails the request (`candidates_for`,
  `scheduler.rs`) instead of spilling. A new init generation mid-stream is not
  possible: the VOD playlist has one `EXT-X-MAP` and clients do not reload it
  (research R2).
- **Eligibility keys off shared-init fMP4, not H264** (`shared_init_fmp4`,
  research R6). mpegts H264 repeats parameter sets per segment and keeps free
  load-balancing (FR-006).
- **Placement is the cache's business too.** `HLS_GEN_VERSION` bumps whenever the
  assignment rule changes (V89), and shared-init URIs carry it (V90). The
  generation is a path namespace, not a wipe-on-boot flag (V149).

Hardware self-consistency was measured before any code shipped: three
independent `h264_nvenc` encodes on a GTX 1070 produced byte-identical `avcC`,
while libx264 differed (Main vs High, `log2_max_frame_num` 8 vs 4; research R4,
R7). No flag or avcC rewrite reconciles them.

## Consequences

- Browser CMAF H264 reaches the GPU. Success criteria SC-001 to SC-004 name the
  queries. Per-device speed against CPU was later re-measured under spec 007,
  which now spreads renditions by weight across devices; the placement function
  is the extended form of this ADR's rule.
- A lost pinned device is a visible stall, not silent corruption. hls.js
  retries and a fatal error reloads the source, which re-pins. This was chosen
  over undecodable video under a 200.
- Three production escapes, each a placement input that varied inside one
  rendition or a cache that outlived a rule change:
  - B133: browser cache held the old init (V90).
  - B135: a revert-of-revert would have re-landed the rule under a stale generation.
  - B196/B200: a rolling deploy and the per-segment burn gate each split one
    rendition across encoders (V149, V153).
- Adding or removing a GPU can change a rendition's device. The placement
  fingerprint feeds the generation so cached segments are invalidated.
- Out of scope and unchanged: VP9 stays on CPU (no NVENC VP9 encoder, no VAAPI
  on this host).

## Alternatives considered

- **Silent CPU fallback when the device is lost.** That is #114 (research R2).
- **Live-playlist rewriting with `EXT-X-DISCONTINUITY` and a second `EXT-X-MAP`.**
  Turns every VOD playlist mutable and needs client behaviour we do not control.
- **Make the encoders agree** (flags, `avcC` rewrite, libopenh264). Slice-header
  field widths differ, so it cannot be patched in the container (research R7).
- **Pin map keyed by rendition.** Superseded by the pure function (research R8).

## References

- ADR-0005 (per-segment transcode; this ADR adds the one-encoder rule for CMAF)
- `specs/003-cmaf-hw-affinity/` (spec.md Problem and FR-001..006, plan.md, research.md R1-R8)
- `specs/001-pharos-baseline/invariants.md` V89, V90, V149, V153; `bugs.md` B133, B135, B196, B200
- `crates/pharos-transcode/src/device.rs` (`device_supports`, `rendition_device`),
  `scheduler.rs` (`candidates_for`), `crates/pharos-cache/src/hls_cache.rs` (`HLS_GEN_VERSION`)
