# Tasks: a Rust aligner service (039)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0116. Ids are local to this spec (`T039-NN`). Every code task is test first. No media titles, paths or ids in code, tests or fixtures.

## Slice 1 - CTC, text and tokens (V1270, V1271, V1252), built on `feat/rust-aligner-ctc`

- [x] T039-01 `crates/pharos-align`: `text::words` matches Python on every assigned code point, its whitespace set and mixed strings (V1270).
- [x] T039-02 `tokens`: the MMS_FA labels and tokenizer, matched against torchaudio's dictionary.
- [x] T039-03 `ctc`: torchaudio 2.11 `forced_align`, `merge_tokens` and the per-word split. Paths and log scores match bit for bit on 47 synthetic emissions; infeasible targets error (V1271).
- [x] T039-04 `line`: `_align_loaded` after the model, and frame-to-ms (V1252). Locally, real torch emissions for two songs give 658 words with Δ 0.

## Slice 2 - Pipeline and service (V1253-V1256)

- [x] T039-05 `plan`: windows with the lead clamp, stamps, `parse_request`, `resolve`, with fixtures from `plan.py` (V1253).
- [x] T039-06 `offset`: probe choice, estimate and shift, with fixtures from `offset.py` (V1254).
- [x] T039-07 `audio`: `ffmpeg` per window with Python's arguments and timeout; a test asserts the exact argv.
- [x] T039-08 `bin/pharos-aligner`: `/align`, `/healthz`, `/metrics` and the error statuses (V1255); the one-at-a-time lock with an `aligner.wait` span; idle unload; env parsing with errors naming the value.
- [x] T039-09 OTLP spans and the song's log line (V1256). A test asserts the span names and attributes. Disarm it once to see it fail.

## Slice 3 - CPU forward pass (V1257-V1260)

- [x] T039-10 Weight conversion derivation (`weights.bin`, `weights.json`) in the flake. A check asserts the tensor count (422) and the total f32 count.
- [x] T039-11 `weights`: mmap, index lookup and shape checks. A missing or short tensor is an error naming it.
- [x] T039-12 `forward::cpu` from the `cpufwd` spike, plus log_softmax and the star column (R9). Emission fixtures from torch on synthetic waveforms, max abs 2e-2, identical CTC paths (V1257).
- [x] T039-13 AVX2 check at start, with a refusal naming the feature (V1258).
- [x] T039-14 Pinning `auto`/`on`/`off` from sysfs topology. Unit tests on synthetic sysfs trees: one L3 with no SMT gives no pinning; two L3 domains plus SMT gives one thread per core in the largest domain (V1259).
- [x] T039-15 Batch cap for the offset probes. RSS test on the synthetic songs at or below 2.6 GiB (V1260).

## Slice 4 - Vulkan forward pass (V1261-V1263)

- [x] T039-16 `forward::vulkan` in Burn ops from `weights.bin`, f32, same emission fixtures and tolerance (V1261); burn-onnx fallback only if this misses.
- [x] T039-17 `device`: the choice and reasons (R10, R11), with llvmpipe refused. Reasons are asserted in tests with a fake adapter list (V1262).
- [x] T039-18 Runtime fallback per song and the 3-in-10-minutes latch, `pharos_aligner_device_fallback_total{reason}` (V1263).

## Slice 5 - Warm-up and cache (V1264)

- [x] T039-19 `PHAROS_ALIGNER_CACHE_DIR` wired to cubecl persistence, plus warm-up lengths, before readiness. Start log fields (V1264).
- [x] T039-28 `pharos_aligner_forward_seconds{device}` histogram and `aligner.rss_mib` on the song's log line, both named in spec.md's signals table. Emitted by the implementation PR, each with a test that fails without it.
- [ ] T039-20 Measure the cold cost per window length on the RX 9070 node, and settle open question 1. Results go in this file, with ISO 8601 UTC times.

## Slice 6 - Image and chart (V1265-V1266)

- [x] T039-21 nix OCI image `pharos-aligner` from the Rust binary, with OpenBLAS, ffmpeg-headless, vulkan-loader, Mesa RADV and the weights. Image size recorded against the Python image. Built 2026-11-10T14:38Z as `.#alignerRsOci` (image `pharos-aligner-rs`): 2.83 GB, 1.2 GB of it weights; the Python aligner's closure is 3.5 GiB.
- [x] T039-22 Chart values: `aligner.device`, `aligner.gpu.*`, `aligner.cache.*`, `aligner.shadow.*`. `helm template` tests for the GPU on and off and the shadow on and off (V1265).
- [x] T039-23 Readiness waits for warm-up, and the GPU pod gets the render group (V1266).

## Slice 7 - Shadow and cutover (V1267-V1269)

- [x] T039-24 Server: `[lyrics].aligner_shadow_url`, one in flight, the word-by-word comparison, the log line and `pharos_lyrics_shadow_total{outcome}` (V1267).
- [ ] T039-25 Shadow week on the node. Report the match rate, device split, forward seconds per device and RSS, and the pinning `on` vs `off` result for R18.
- [ ] T039-26 Cutover: `aligner_url` points to the Rust service; every answer the server receives carries `aligner_version` 5 (V1268).
- [ ] T039-27 Remove the Python service: `aligner/`, the flake outputs `aligner`/`alignerOci`, and the CI job (V1269).
