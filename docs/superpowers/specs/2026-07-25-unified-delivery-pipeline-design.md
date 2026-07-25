# Unified segmented-delivery pipeline — design

**Status:** approved 2026-07-25. Supersedes the per-path structure of
`api::jellyfin::hls`.

**Goal:** make the class of bug that has recurred eleven times structurally
impossible, and fix the live audio defect that is the eleventh instance.

---

## 1. Problem

pharos serves segmented video over three near-identical delivery paths:

| Route | Container | Video | Audio | Clients |
|---|---|---|---|---|
| `/videos/{id}/hls1/{variant}/{n}.ts` | mpegts | h264 | **muxed, re-encoded per segment** | native (Android/Google TV) |
| `/videos/{id}/h264cmaf/{n}.m4s` | fMP4 | h264 | separate rendition | browsers |
| `/videos/{id}/vp9/{n}.m4s` | fMP4 | vp9 | separate rendition | Firefox (legacy) |

Each has its own playlist builder, segment handler, init handler and
opts builder. They are not abstractions over a shared core; they are three
copies that drifted.

### 1.1 Evidence — every bug was a duplicated decision

Eleven shipped defects, grouped by *which* decision was re-derived:

**Timeline** (4)
- Segment grid computed in five places → a frame fell between segments and was
  dropped, at nearly every boundary (PR#80).
- Frame rate validated in neither prober → the MPEG-TS 90 kHz container clock
  was accepted as a frame rate, silently flattening the grid on 479 items
  (PR#79).
- Tail clamp + segment count re-derived in four playlist builders (PR#82).
- The audio rendition invented a sixth grid, so two sessions wrote different
  bytes under one filename (PR#82).

**Identity** (2)
- Cache key drew its bitrate solely from the video bitrate, so all five rungs
  of the audio ladder collapsed onto one entry (PR#81).
- The ETag restated the key from a hand-picked argument list and drifted from
  it (PR#81).

**Encode recipe** (2)
- `-enc_time_base` wired for fMP4 only; mpegts segments never got frame-exact
  timestamps (PR#79).
- `-output_ts_offset` gated on a non-zero start, so segment 0 alone is
  unanchored and lands ~21 ms late (open, documented).

**Source facts** (2)
- `AV_DISPOSITION_ATTACHED_PIC` unchecked in both probers (PR#81).
- Trickplay height rounded one way for the DTO and another by the scaler
  (PR#81).

**Audio packaging** (1 — open, this spec fixes it)
- The continuous-audio rendition was applied to two of the three paths.

### 1.2 The live defect

The muxed mpegts path AAC-encodes audio *independently per segment*
(`build_segment_opts` sets `audio: Some(SegmentAudio::Aac)`), so every segment
carries a fresh encoder priming frame and restarts the AAC frame grid at its own
seek point. Measured on three consecutive live segments of Fringe S01E02:

```
seg40  video [239.969 → 245.975]   audio [239.9477 → 245.985]
seg41  video [245.975 → 251.981]   audio [245.9537 → 251.991]
seg42  video [251.981 → 257.987]   audio [251.9597 → 257.997]
```

Each segment holds **6.037 s of audio against 6.006 s of video**, so ~31 ms is
duplicated at every boundary. The copies are not interchangeable: consecutive
segments' AAC grids are offset by 0.53 of a frame (**11.3 ms**), because
`(245.953667 − 239.947667) / 0.0213333 = 281.53`, not an integer. Reported
symptom: intermittent freeze-then-catch-up on the Google TV app. The browser
path is unaffected — it was given a separate continuous-audio rendition
precisely to eliminate per-segment priming.

### 1.3 Why "add a trait" is not by itself the answer

A `trait DeliveryPath` with ten methods each path implements reproduces the
duplication with ceremony, and makes it look deliberate. Traits model genuine
variation; every bug above was an **invariant** that leaked into the variation
slot.

Governing rule for this design:

> If a path can compute it, a path can compute it wrongly.

So the shared decisions must not be *reachable* from a path, and the trait
surface must cover only what genuinely differs.

---

## 2. Goals / non-goals

**Goals**
1. One definition each of: the segment grid, the encode recipe, the cache
   identity, the playlist body.
2. Audio encoded **once per title**, never per segment, for every path.
3. The trait surface reduced to the single real difference between paths.
4. Adding a codec/container becomes a table row, not a handler.

**Non-goals** (explicitly out of scope, do not touch)
- DirectPlay / progressive `/videos/{id}/stream`.
- Subtitle *delivery* (external/Stream.js). Subtitle *burn-in* stays as-is.
- SyncPlay, trickplay, images, waveform.
- Client-visible URLs. Every existing route keeps its path and shape, so no
  client needs to change and the deploy is not a client-compat event.

---

## 3. Architecture

### 3.1 `SegmentPlan` — computed once, never by a path

A single constructor turns `(item, profile, index)` into everything a segment
needs. Paths receive it and cannot build one field themselves.

```rust
pub struct SegmentPlan {
    index: SegmentIndex,
    start_secs: f64,
    duration_secs: f64,
    opts: SegmentOpts,
    identity: SegmentIdentity,
}
```

`SegmentOpts`' fields become private, with `SegmentPlan` the only constructor.
That retires the Timeline and Encode-recipe classes at once: a handler cannot
pass a start position, because it never holds one.

**Crate placement.** `SegmentGrid` / `segment_range` / `SEGMENT_SECONDS` move
from `pharos-server::api::jellyfin::seek` into `pharos-core` (they are pure
arithmetic over `pharos_core::FrameRate`). `SegmentPlan` lives in
`pharos-transcode::segment` beside `SegmentOpts`. Both `pharos-cache` and
`pharos-server` then reach one implementation; today `pharos-cache` cannot see
the grid at all, which is exactly why the audio rendition grew its own.

### 3.2 Delivery profile as data

```rust
pub struct DeliveryProfile {
    pub container: SegmentContainer,     // Mpegts | Fmp4
    /// `None` = audio-only item (the `/hls1/{A64..A256}` ladder for music).
    pub video: Option<SegmentVideo>,     // H264 | Vp9
    pub audio: AudioDelivery,
}

pub enum AudioDelivery {
    /// Muxed into each media segment — but the bytes are COPIED from the
    /// title's one continuous audio encode, never re-encoded here.
    Muxed(ContinuousAudio),
    /// Served as its own rendition (`EXT-X-MEDIA` group).
    Separate(ContinuousAudio),
}
```

There is deliberately **no variant meaning "encode audio for this segment"**.
The live defect becomes a value that cannot be constructed.

The three paths collapse to a table:

| Profile | container | video | audio |
|---|---|---|---|
| `NATIVE_TS` | Mpegts | H264 | `Muxed(Aac)` |
| `WEB_H264` | Fmp4 | H264 | `Separate(Opus)` |
| `WEB_VP9` | Fmp4 | Vp9 | `Separate(Opus)` |
| `AUDIO_ONLY` | Mpegts | — | `Muxed(Aac)` |

The audio-only ladder is a real delivery path with its own bug history (its five
rungs shared one cache entry until PR#81), so it is a profile row rather than a
special case. It benefits from the same continuous encode: its rungs differ only
in bitrate, which is part of the continuous-encode key.

### 3.3 Continuous audio — one encode, two packagings

One audio encode per `(media, track, bitrate, codec)`, materialised on disk by
the existing session machinery (already correct after PR#82: per-session
directories, deterministic resolution, uniform grid).

- `Separate` — served as today's HLS rendition. Unchanged behaviour.
- `Muxed` — the segment ffmpeg takes the continuous encode as a **second
  input** and `-c:a copy`s the slice into the segment:

```
ffmpeg -ss START -i SOURCE -ss START -i CONTINUOUS_AUDIO \
       -map 0:v:0 -map 1:a:0 -c:v <enc> -c:a copy -f mpegts ...
```

Because the audio grid is global, every frame belongs to exactly one segment at
a deterministic PTS. The boundary frame may appear in two adjacent segments, but
**byte-identical and at the same PTS** — which a player resolves by overwrite.
That is categorically different from today's 31 ms of differently-encoded,
phase-shifted overlap.

Extending the continuous encoder to emit AAC (it is Opus-only today) is
required; the codec becomes a parameter of the session key and its output
directory.

### 3.4 The trait — one method

After 3.1–3.3 the paths differ in exactly one behaviour: how encoder output
reaches the wire.

```rust
pub trait SegmentPackaging {
    fn content_type(&self) -> &'static str;
    /// mpegts: pass through. fMP4: split `ftyp+moov` from `moof+mdat`.
    fn finish(&self, raw: Vec<u8>) -> Result<Packaged, PackagingError>;
}
```

### 3.5 Identity derived, not restated

```rust
fn segment_identity(media_id, index, profile, audio_track, burn, bitrate)
    -> SegmentIdentity
```

Cache filename **and** HTTP ETag both derive from this one value, so they cannot
drift (PR#81 fixed the symptom; this removes the possibility).

### 3.6 One playlist renderer

`render_vod_playlist(grid, profile, urls)` replaces four builders. It already
has the right shape after PR#82's `playlist_segments`.

---

## 4. Testing strategy (TDD)

The characteristic failure of this codebase is a test that encodes the bug —
the original boundary test asserted frame alignment against the same decimal fps
it was validating, so it passed on a drifting grid. Tests here must assert
against **independently derived** values, and the load-bearing ones must be
measured from real encoder output rather than from our own arithmetic.

**Crown-jewel test, written first, must fail on current code:**

`audio_frames_tile_exactly_across_a_segment_boundary` — encode two adjacent
segments from a fixture through the real pipeline, then assert:
1. every audio frame PTS in segment N+1 is ≥ the last PTS in segment N;
2. the inter-segment PTS delta equals the codec frame duration (one frame), not
   a fraction of one;
3. `(first_pts(N+1) − first_pts(N)) / frame_duration` is an **integer** — the
   property that is 281.53 today.

Assertion 3 is the exact defect and cannot be satisfied by a per-segment
encode.

**Supporting tests**
- `SegmentOpts` cannot be constructed outside `SegmentPlan` (compile-fail test).
- Grid property test: for a range of frame rates and indices, consecutive
  segments tile with no gap and no overlap.
- Identity: cache filename and ETag change together, for every field.
- Golden argv per profile, including that `Muxed` emits `-c:a copy` and two
  inputs, and that **no profile emits an audio encoder for a segment**.
- Playlist: all three profiles round-trip through one renderer.

**Regression guard**: existing tests for the three paths must keep passing
unchanged where they assert client-visible output (playlist shape, content
types, URLs). Any such test that must change is a signal to stop and re-read —
per §1.3 the client contract is a non-goal to alter.

---

## 5. Risks

| Risk | Mitigation |
|---|---|
| mpegts is the only native-client path; a regression is a total outage for the TV app | Container, codecs and URLs are unchanged — only the origin of the audio bytes changes. Crown-jewel test proves continuity before merge. |
| Continuous AAC encode adds a second ffmpeg per title | Same machinery, cost and lifecycle as the existing Opus rendition, which is already proven in production. |
| Deep seek needs audio available at the target | Already solved by the seek-session logic (PR#82); reused unchanged. |
| Big-bang merge | Explicitly requested to avoid repeated build/deploy cycles. Mitigated by TDD and a full `just test` + clippy gate. |

**Rollback:** revert the merge commit. No schema, cache-format or URL change —
`HLS_GEN_VERSION` bumps so segments regenerate, which is self-healing in both
directions.

---

## 6. Success criteria

1. The crown-jewel test fails on `main` today and passes after.
2. Fringe S01E02 plays without freezes on the Google TV app.
3. `grep` finds one segment-grid definition, one identity function, one playlist
   renderer, one audio encoder.
4. No route, content type or playlist field changes.
