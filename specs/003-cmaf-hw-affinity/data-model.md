# Data Model: CMAF rendition affinity

All state is in-process. Nothing is persisted: a pin describes an encoder choice
for a live rendition, and a restarted process must not inherit a claim it cannot
verify (see R2 — an unknown pin is treated as a fresh rendition).

## RenditionKey

Identifies a set of segments that must share one encoder because they share one
init. Derived, never hand-assembled (R1).

| Field | Source | Why it is in the key |
|---|---|---|
| `input` | `TranscodeOptions.input` path | different source, different SPS |
| `video` | `Option<VideoCodec>` | codec determines the parameter set |
| `container` | `Container` | the hazard is scoped to shared-init fMP4 (R6) |
| `video_bitrate_bps` | rung | rate control affects SPS/VUI |
| `audio`, `audio_bitrate_bps`, `audio_source_stream_index` | audio variant | a different audio variant is a different init |
| `burn_subtitle_stream_index`, `burn_subtitle_is_text` | burn variant | burn changes the video stream |
| `source_frame_rate` | probe | carried into `-r`, affects timing/VUI |

**Excluded**: `start_position_ticks`, `duration_ticks` (they are what varies
*within* a rendition), and `PlaySessionId` (two sessions on identical options
should share cache, R1).

**Validation**: the key MUST be computed from the same `TranscodeOptions` value
used to build the encode, so a new option field cannot silently fall out of it.

## RenditionPin

| Field | Type | Notes |
|---|---|---|
| `device` | `DeviceId` | chosen once, at init time (R5) |
| `pinned_at` | `Instant` | for idle eviction |

**Lifecycle**

1. **Unpinned** → init (segment 0) is requested. Placement picks the least-loaded
   eligible device; the choice is recorded.
2. **Pinned** → every later segment of the key dispatches to `device` only. If the
   device has no free permit, the job QUEUES on it. It never spills (FR-001).
3. **Device unavailable** (cooldown / repeated failure) → the pin is invalidated
   and the request FAILS. The client restarts the stream and re-pins (R2). The
   server never continues the same generation on another encoder.
4. **Idle** → evicted after a TTL comfortably beyond a segment fetch gap, so a
   finished rendition does not hold a device preference forever.

**Invariant (the one that matters)**: for a given `RenditionKey`, the set of
devices that produced its delivered segments has cardinality ≤ 1 — including
segments served from disk cache (R3).

## Cache identity

The on-disk segment path gains the pinned device (R3). `HLS_GEN_VERSION` is bumped
so the existing CPU-only cache cannot be re-served under a hardware init.
