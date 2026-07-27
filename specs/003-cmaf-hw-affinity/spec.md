# Feature Specification: hardware encoding for CMAF renditions

**Feature Branch**: `003-cmaf-hw-affinity`
**Created**: 2026-07-27
**Status**: Draft
**Input**: Group watch of *Hoppers* (3 members) buffered repeatedly and wedged the
SyncPlay group, while the GPU sat idle. Measured: 420 of 423 transcode jobs ran on
CPU at 1.81× realtime; NVENC ran 3, at 1.9× the speed, with `capacity 8`,
`in_use 0`, `cooldown 0` throughout.

## Problem

`device_supports` (`crates/pharos-transcode/src/device.rs:167`) makes every
hardware encoder ineligible for H264 in fMP4:

```rust
Some(VideoCodec::H264) if opts.container == Container::Fmp4 => false,
```

The constraint behind it is real and was an outage (issue #114): a shared-init
CMAF rendition serves every segment under one `avcC` carrying seg0's SPS, and the
media segments carry no inband parameter sets. libx264 and the hardware H264
encoders emit incompatible SPS — differing in the slice-header determinants (POC
type, `log2_max_frame_num`, reference structure) that no `-profile` / `-level` /
`-refs` flag can align. A segment the load-balancer spilled to a different encoder
than seg0 is undecodable under that init.

The requirement is therefore **"every segment of a rendition comes from one
encoder"**. CPU-only is one way to satisfy it. It is not the only way, and it has
become expensive: PR #70 flipped all browser H264 to CMAF, so on an NVENC-only
host every browser path is now CPU-bound —

| Browser path | Encoder | Why |
|---|---|---|
| H264 CMAF | CPU only | the rule above |
| VP9 | CPU only | NVENC has no VP9 encoder; no VAAPI on this host |

leaving the GPU reachable only by mpegts H264, i.e. native apps. Three browser
viewers exhaust the 4 CPU permits, encoding drops toward realtime, members buffer,
and the SyncPlay readiness gate freezes the group (`readiness gate timed out;
anti-wedge`, observed 2026-07-27T22:12Z).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A group watch on browsers uses the GPU (Priority: P1)

Three people watch one film together in browsers. The server encodes the rendition
on the GPU, keeps well ahead of realtime, and nobody buffers.

**Acceptance**: with a hardware device present and healthy, a browser CMAF H264
rendition dispatches to that device, and every segment of the rendition — init and
all media segments — comes from it.

### User Story 2 - A rendition never mixes encoders (Priority: P1)

The #114 corruption must not return. This story is the guard on Story 1, not a
nice-to-have: it is the reason the CPU-only rule exists.

**Acceptance**: once a rendition's first segment is produced on a device, no later
segment of that rendition is produced on a different device — including when the
scheduler is under load, when the preferred device is saturated, and when
prefetch and interactive jobs for the same rendition race.

### User Story 3 - Losing the device degrades safely (Priority: P1)

A hardware device can enter cooldown or fail mid-rendition. Silently continuing on
another encoder is exactly the #114 bug.

**Acceptance**: when a rendition's pinned device becomes unavailable, the server
either (a) waits for it, or (b) starts a NEW rendition generation with its own
init, so no client ever reads segments from two encoders under one init. Which of
these is chosen is a design decision for `plan.md`; both are acceptable, silent
fallback is not.

### Edge Cases

- Pinned device saturated while CPU is free — must NOT spill (Story 2).
- Server restart mid-rendition: the pin is in-memory; a new process must not
  assume the previous encoder. Treat an unknown pin as a new generation.
- Two different renditions of the same item (h264cmaf + vp9) pin independently.
- Cache hits must not be attributed to a device or disturb the pin.
- mpegts H264 must keep today's free load-balancing — it repeats parameter sets
  per segment and is self-describing, so it carries no such hazard.
- H265 fMP4 would carry the identical hazard; no such rung exists today, but the
  mechanism must not be H264-specific by accident.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A CMAF H264 rendition MUST be produced entirely by one encoder.
- **FR-002**: Hardware devices MUST be eligible for CMAF H264 when the
  one-encoder guarantee is enforced by other means.
- **FR-003**: The identity a pin is keyed by MUST distinguish renditions that
  cannot share an init — at minimum item, codec, container, bitrate rung, and
  audio/subtitle variant.
- **FR-004**: Loss of a pinned device MUST NOT result in segments from a second
  encoder under the same init.
- **FR-005**: The device chosen for a rendition, and any change of generation,
  MUST be observable — a log line carrying the rendition key, the device, and the
  reason, plus a metric distinguishing pinned dispatch from free dispatch.
- **FR-006**: mpegts H264 behaviour MUST be unchanged.

### Key Entities

- **Rendition key** — identifies a set of segments that share one init.
- **Rendition pin** — rendition key → device, plus a generation counter.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With a healthy NVENC device, ≥90% of browser CMAF H264 segment
  encodes dispatch to hardware (today: 0.7% of all jobs, and 0% of CMAF).
  Query: `sum by (device) (pharos_segment_produced_total)` and the
  `transcode_job` span's `device` field.
- **SC-002**: Median encode time for a browser rendition falls to hardware
  levels (measured: 1825 ms vs 3380 ms per 6 s segment — 3.3× vs 1.8× realtime).
- **SC-003**: Zero segments of any single rendition generation come from more
  than one device, asserted by test under induced cooldown and saturation.
- **SC-004**: Three concurrent browser viewers of one title do not exhaust
  transcode capacity: no `outcome="shed"` on interactive jobs, and no SyncPlay
  `readiness gate timed out` attributable to buffering.

## Assumptions

- The #114 analysis is correct and unchanged: hardware and software H264 SPS are
  not reconcilable by encoder flags. This spec does not attempt to make them
  compatible; it isolates them per rendition instead.
- NVENC session budget (8) exceeds plausible concurrent rendition count on this
  deployment; contention policy is a `plan.md` concern.
- VP9 remains CPU-bound on this host regardless — no NVENC VP9 encoder, no VAAPI.
  Halving the browser ladder split is out of scope here.

## Out of Scope

- Making VP9 hardware-encodable (needs a VAAPI-capable device).
- Changing which codec a browser negotiates (`002` / capability negotiation).
- The SyncPlay gate behaviour itself — it responded correctly to real buffering.
