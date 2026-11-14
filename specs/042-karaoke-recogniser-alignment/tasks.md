# Tasks: karaoke lines placed by what the recogniser heard (042)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0124, ADR-0125. Ids are local to this spec (`T042-NN`). The slices stack: each branch carries the one before it, so the PRs open in slice order. Fixtures are invented; no real titles, artists or paths.

## Slice 0: signals on today's path

Branch `feat/karaoke-anchor` (first two commits).

- [x] T042-01 Count how each lyric line is placed and why a song falls back (`522786756`).
- [x] T042-02 Record today's lines as `kept` with the recogniser off (`8795db418`).

## Slice 1: transcript, anchor, refine

Branch `feat/karaoke-anchor`, head `9a5bfca72`. Ids B1403, V1373, V1397, V1398.

- [x] T042-03 Read a recogniser transcript of timed words, times as the nearest double (`59131e6f1`, `e80d38533`).
- [x] T042-04 Anchor lyric lines on the recogniser's words; place lines from anchors on the emission (`03c567b29` to `77fc6d0ef`).
- [x] T042-05 Align the anchored span in one pass; drop an outlying anchor (`f060b0b9a`, `05cdc0d5a`).
- [x] T042-06 Lines outside the anchored span never overlap it (V1373); an anchor is set aside only when it leaps a sung line (V1397).
- [x] T042-07 A line after a long gap stays near its expected time (B1403, V1398).
- [x] T042-08 Parity against the Python prototype on the private set, run on the desktop; results not committed.

## Slice 2: recogniser

Branch `feat/karaoke-recognise`, head `1ca87ef79`. Id V1399.

- [x] T042-09 Spike: `whisper-rs` on Vulkan against Burn; went with whisper.cpp (ADR-0124).
- [x] T042-10 `whisper-rs` behind the `recognise` feature; whisper.cpp's build tools in the flake (`38d794890`, `2a3c2e6e4`, `1ca87ef79`).
- [x] T042-11 Timed words from whisper tokens; lost windows retried, counted (`422b0ea38`, `456606411`); a retry replaces a window only with a passage (V1399).
- [x] T042-12 whisper large-v3-turbo through whisper.cpp, and a `recognise` example (`4a5fc6c38`, `1c500601f`).

## Slice 3: separator

Branch `feat/karaoke-separate`, head `df0b5d1c3`. Id V1400.

- [x] T042-13 Spike: htdemucs's ONNX export imported into Burn at build time (ADR-0125, `9afba31a6`).
- [x] T042-14 Spectrogram and its inverse; segment-by-segment separation as demucs does (`6e5af96e7`, `7e3dbba37`).
- [x] T042-15 With no GPU the recogniser hears the mix (V1400, `60626b57b`). Superseded by T042-23: without the Vulkan GPU the stages do not run and the song keeps today's answer.
- [x] T042-16 htdemucs on the GPU behind the `separate` feature, a `separate` example, one neutral outcome log (`656e38524` to `b0f9ac750`).
- [x] T042-17 Reproducible weights: no ONNX copy in the closure, random param ids stripped (`47fb7ab10`, `df0b5d1c3`).

## Slice 4: service wiring

Branch `feat/karaoke-service`, head `a8f4cfe24`. Ids V1401, B1427, B1428, B1435, V1409.

- [x] T042-18 A model hands back a window's emission; whole-song emission stitched from 30 s chunks (`3a8334dea`, `2f2f6932c`).
- [x] T042-19 Whole song decoded in stereo, resampled to mono; separator GPU memory released per song (`3b5ec39f7`, `18e40bbfa`).
- [x] T042-20 Decode and emission failures named; placement serialised; lines placed by what was heard (`120848f99`, `c46e3db34`).
- [x] T042-21 Lyrics service takes aligner 6's corrected lines and placement; corrected lines follow today's answer (V1401, `fb1031662`, `f384eccae`).
- [x] T042-22 Stages on the real models, switched on by model path, built into the aligner image (`f2f538d21`, `d390da57e`, `94187a531`).
- [x] T042-23 Stages only on the Vulkan GPU, else `no_gpu_stages`; a time budget well under the request timeout, else `stage_timeout` (B1428, `59b47cc84`, `032ec1a0e`).
- [x] T042-24 Crash-loop guard: a song that aborted the process skips the stages, `stage_crashed`, counted (B1435, V1409, `af4b46a5e`).
- [x] T042-25 One cubecl stream shared by every thread, so the kept pool stops growing per worker thread (B1427, `bef22b2b3`).
- [x] T042-26 B1427, B1428, B1435 and V1409 recorded (`a8f4cfe24`).
- [ ] T042-30 Boundary hold: when taking an edge line into the span makes the joint pass infeasible, hold that line's word starts at the placed edge, then walk the hold outward through the kept lines beyond it until one already keeps order, so word starts never decrease (V1373, plan step 6).
- [ ] T042-31 Mark V1400 superseded by V1409 in `invariants.md`; remove the mix path if nothing reaches it.

## Slice 5: deploy and verify

- [ ] T042-27 Deploy the aligner image; `cache_dir` on the chart's persistent volume so the crash list and kernel cache survive restarts.
- [ ] T042-28 Owner's OK, then re-align the library.
- [ ] T042-29 Verify by query: placement and fallback counts, `stage_timeout` / `stage_crashed` rates, kept-pool gauge flat across songs.

## Related

- `fix/karaoke-weak-stamps` (`f28b7fe0f`, B1402, V1372): the weak-run guard on today's path, separate from this stack.

## Open

- A cold first song can overshoot the stage budget by one segment while kernels compile (seen 41.5 s against 15 s); well inside the request timeout.
- A few songs give different answers between runs on the same build; not yet explained.
