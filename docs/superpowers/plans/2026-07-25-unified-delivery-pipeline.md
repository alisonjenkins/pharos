# Unified segmented-delivery pipeline — implementation plan

> Spec: `docs/superpowers/specs/2026-07-25-unified-delivery-pipeline-design.md`

**Goal:** one definition each of the segment grid, encode recipe, cache identity
and playlist body; audio encoded once per title for every path; the live
per-segment-AAC defect made unrepresentable.

**Architecture:** shared grid lowered into `pharos-core`; `SegmentPlan` in
`pharos-transcode` as the sole constructor of `SegmentOpts`; delivery paths
become rows of a `DeliveryProfile` table; the only trait is packaging.

**Tech stack:** Rust, actix-web, ffmpeg (NVENC in prod), `cargo nextest`.

## Global constraints

- **No client-visible change.** Routes, content types, playlist fields and
  codecs stay exactly as they are. Any test asserting client-visible output that
  starts failing is a stop-and-re-read signal, not a test to update.
- **Ship as ONE unit** (explicit user instruction — avoids repeated
  build/deploy cycles). One branch, one PR, atomic commits within it.
- Every commit compiles and passes `just test` on its own.
- `nix develop --command` for all cargo/ffmpeg invocations.
- Run `cargo fmt --all` before each commit; pre-commit only *checks*.
- Clippy `--release --workspace --all-targets` must be clean before the PR.
- `HLS_GEN_VERSION` bumps once, in the commit that changes segment bytes.
- Never delete an existing regression test's intent; if one must change, quote
  it in the commit body and justify.

---

## Task 1: Move the segment grid into `pharos-core`

**Why first:** `pharos-cache` cannot see the grid today, which is the direct
cause of the audio rendition growing a sixth one.

**Files:**
- Create: `crates/pharos-core/src/segment_grid.rs`
- Modify: `crates/pharos-core/src/lib.rs` (module + re-export)
- Modify: `crates/pharos-server/src/api/jellyfin/seek.rs` (re-export, delete the moved code)

**Interfaces produced:** `pharos_core::{SEGMENT_SECONDS, SegmentGrid, SegmentIndex, segment_range, frame_snapped_start, segment_seek_bias}`

- [ ] **Step 1: move the code verbatim.** Cut `SEGMENT_SECONDS`,
  `frame_snapped_start`, `segment_seek_bias`, `segment_range`, `SegmentIndex`,
  `SegmentGrid` out of `seek.rs` into the new module, unchanged. Leave
  `pub use pharos_core::{...};` in `seek.rs` so every existing call site keeps
  compiling untouched.
- [ ] **Step 2: move their tests too**, unchanged. They are the regression net
  for the move; changing them and the code together would prove nothing.
- [ ] **Step 3:** `nix develop --command cargo nextest run --workspace` —
  expect all green with zero source edits outside the two files.
- [ ] **Step 4: commit** `refactor(core): lower the segment grid into pharos-core`.

## Task 2: The crown-jewel failing test

**Why now:** it must fail against the CURRENT muxed pipeline, before any audio
change, or it proves nothing.

**Files:**
- Create: `crates/pharos-transcode/tests/segment_audio_continuity.rs`

- [ ] **Step 1: write the test.** Build a fixture with `ffmpeg lavfi`
  (`testsrc2=rate=24000/1001` + `sine=48000`, 40 s). Encode segments 3 and 4
  through the real argv builder for the `NATIVE_TS` profile. Probe audio frame
  PTS from both. Assert:
  1. `first_pts(4) >= last_pts(3)` — no overlap;
  2. `first_pts(4) - last_pts(3) == frame_duration` within 1 ms — exactly one
     frame, no gap;
  3. `(first_pts(4) - first_pts(3)) / frame_duration` is within 1e-6 of an
     integer — the phase property that measures 281.53 in production.
  Derive `frame_duration` from the probed sample rate (`1024 / rate`), not from
  a literal, so the test cannot encode the bug it is checking.
- [ ] **Step 2: run it, confirm it FAILS**, and record the measured numbers in
  the test's doc comment.
  `nix develop --command cargo nextest run -p pharos-transcode --test segment_audio_continuity`
  Expected: assertion 3 fails with a non-integer ratio.
- [ ] **Step 3: commit** the failing test marked `#[ignore = "fails until the
  continuous-audio muxing lands (Task 5)"]` so the suite stays green mid-plan.
  Commit: `test(transcode): prove per-segment AAC restarts the frame grid`.

## Task 3: `AudioDelivery` + `DeliveryProfile`

**Files:**
- Modify: `crates/pharos-transcode/src/segment.rs`

**Interfaces produced:** `DeliveryProfile`, `AudioDelivery::{Muxed, Separate}`, `ContinuousAudio`, `profile::{NATIVE_TS, WEB_H264, WEB_VP9, AUDIO_ONLY}`

- [ ] **Step 1:** write tests asserting the table's rows and that
  `AudioDelivery` has no per-segment-encode variant (exhaustive `match` over
  the enum in a test proves the shape).
- [ ] **Step 2:** add the types. `ContinuousAudio { codec, bitrate_bps }`.
- [ ] **Step 3:** run tests; commit
  `feat(transcode): model delivery profiles as data`.

## Task 4: Continuous audio gains a codec

**Files:**
- Modify: `crates/pharos-cache/src/hls_cache.rs`

- [ ] **Step 1:** write a test that `audio_hls_args` for AAC emits `-c:a aac`
  and an ADTS/fMP4 output whose directory key includes the codec, and that the
  Opus row is byte-identical to today's argv (regression guard).
- [ ] **Step 2:** thread `ContinuousAudio` through `ensure_audio_hls_covering`
  and into the session directory key so Opus and AAC renditions cannot collide.
- [ ] **Step 3:** run `-p pharos-cache`; commit
  `feat(cache): let the continuous audio rendition target AAC`.

## Task 5: Muxed audio copies from the continuous encode

**This is the fix.**

**Files:**
- Modify: `crates/pharos-transcode/src/lib.rs` (argv builder: second input + `-c:a copy`)
- Modify: `crates/pharos-transcode/src/segment.rs` (`SegmentOpts` carries the continuous-audio path, not an audio codec)
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (`build_segment_opts`)
- Modify: `crates/pharos-cache/src/hls_cache.rs` (`codec_tag`)

- [ ] **Step 1:** golden-argv test — `Muxed` emits two `-i` inputs,
  `-map 0:v:0 -map 1:a:0`, `-c:a copy`, and **no audio encoder**. Assert no
  profile can emit `-c:a aac`/`libopus` for a segment.
- [ ] **Step 2:** implement. Delete `SegmentAudio` from `SegmentOpts`; the
  builder takes the continuous encode's path.
- [ ] **Step 3:** un-`#[ignore]` the Task 2 test; run it; it must now PASS.
- [ ] **Step 4:** bump `HLS_GEN_VERSION` (segment bytes change).
- [ ] **Step 5:** full `just test`; commit
  `fix(hls): mux audio from one continuous encode, never per segment`.

## Task 6: One identity function

**Files:**
- Modify: `crates/pharos-cache/src/hls_cache.rs` (expose `segment_identity`)
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (ETag derives from it)

- [ ] **Step 1:** test that changing *any* identity input changes both the cache
  filename and the ETag — a loop over field mutations, so a newly added field
  that is forgotten fails the test.
- [ ] **Step 2:** implement; delete the hand-rolled ETag key string.
- [ ] **Step 3:** commit `refactor(hls): derive cache path and ETag from one identity`.

## Task 7: Packaging trait + one playlist renderer

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs`
- Modify: `crates/pharos-server/src/api/jellyfin/fmp4.rs`

- [ ] **Step 1:** test all four profiles round-trip through one
  `render_vod_playlist`, producing byte-identical output to the current
  builders for a fixed item (golden test captured BEFORE the change).
- [ ] **Step 2:** implement `SegmentPackaging` (mpegts passthrough, fMP4 split);
  collapse the four playlist builders into one renderer.
- [ ] **Step 3:** commit `refactor(hls): one playlist renderer, one packaging trait`.

## Task 8: Seal `SegmentOpts`

- [ ] **Step 1:** make `SegmentOpts`' fields private; `SegmentPlan` the only
  constructor. Fix fallout.
- [ ] **Step 2:** full `just test` + clippy; commit
  `refactor(transcode): SegmentPlan is the only way to build SegmentOpts`.

## Task 9: Ship

- [ ] `just test`, clippy `--release --all-targets`, push, PR, rebase-merge.
- [ ] After deploy: re-run the live measurement from the spec §1.2 against
  Fringe S01E02 and confirm the audio ratio is an integer.
