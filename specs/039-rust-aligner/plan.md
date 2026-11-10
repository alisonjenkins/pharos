# Implementation plan: a Rust aligner service (039)

**Spec**: [spec.md](spec.md). **ADR**: ADR-0116.

## Shape

One crate, `crates/pharos-align`, with a library and a binary.

| Module | Holds | Slice |
|---|---|---|
| `text`, `tokens`, `ctc`, `line` | `words()`, the MMS_FA labels, torchaudio's Viterbi and merge, `_align_loaded` after the model | 1 (built) |
| `plan`, `offset` | ports of `plan.py` (windows, lead clamp, stamps, request parsing, path resolution) and `offset.py` (probes, estimate, shift) | 2 |
| `weights` | mmapped `weights.bin` + `weights.json` index, one copy | 3 |
| `forward::cpu` | the hand-written OpenBLAS forward pass (from the scratch `cpufwd` spike), rayon for elementwise work, optional pinning | 3 |
| `forward::vulkan` (feature `vulkan`) | the MMS_FA graph in Burn on `burn-wgpu` (Vulkan, f32), weights uploaded from `weights.bin` | 4 |
| `device` | `auto`/`vulkan`/`cpu` choice, reasons, runtime fallback, warm-up | 4-5 |
| `audio` | `ffmpeg` spawn per window with the Python arguments, 120 s timeout | 2 |
| `bin/pharos-aligner` | actix-web service: `/align`, `/healthz`, `/metrics`; one-at-a-time lock, idle unload, OTLP spans | 2 |

The binary keeps the Python service's name, `pharos-aligner`, so the chart, the image name and the server's config change as little as possible. The crate is `pharos-align`, so it does not collide with the Python package while both exist.

## Weights

`aligner/` already fetches the MMS_FA checkpoint in the flake (`alignerModel`). A build-time derivation converts it into `weights.bin` (f32 little-endian, about 1.26 GB) and `weights.json` (`{name: {off, shape}}`). It resolves the positional conv's weight norm and drops nothing else. The conversion runs torch at build time only; the image has no Python.

Both backends read the same file. The Burn model is written by hand in Burn ops, mirroring the CPU pass, and its weights are uploaded from the mmap. If its parity misses, the fallback is burn-onnx codegen from an ONNX export (spec open question 2).

## CPU forward pass

This is a port of the `cpufwd` spike with its measured choices kept.

- Activations are channels-last `[T, C]`.
- Feature-extractor convs run as tap-split sgemm with no im2col. The positional conv uses im2col per group.
- QKV is fused into one `[3072, 1024]` weight. Bias is pre-filled, and residuals accumulate with sgemm `beta = 1`.
- Attention uses the `gemm` crate, one thread per (window, head) job. GELU, exp and LayerNorm are hand-vectorised.
- OpenBLAS runs with `OMP_WAIT_POLICY=passive`, set by the binary before the first GEMM, so its OpenMP pool does not spin against rayon.
- Target CPU is `x86-64-v3` (AVX2 and FMA). The production host is a Zen 3 EPYC 7543P and has both. The binary checks for AVX2 at start and refuses to run without it, naming the missing feature.

## Vulkan forward pass

- Burn `wgpu` backend, f32 only.
- cubecl's cache directory is set from `PHAROS_ALIGNER_CACHE_DIR` and needs burn's `persistence` feature.
- `vulkan-loader` and Mesa's RADV come from nix and sit in the image.
- The adapter is chosen by wgpu: the first non-CPU Vulkan adapter. llvmpipe and lavapipe are refused.

## Device choice and fallback

1. Read `PHAROS_ALIGNER_DEVICE`.
2. For `auto` or `vulkan`, enumerate the adapters and run the warm-up (R15) on the first usable one.
3. Record the device and reason (R11).
4. During a song, a Vulkan error reruns the song's remaining forwards on the CPU. The model on the CPU side is mmapped, so it is cheap to keep.
5. After 3 errors in 10 minutes, the service stays on the CPU (R12). This threshold is an assumption, to revisit after the shadow week.

## Chart (`charts/pharos/templates/aligner.yaml`)

- The Rust service ships as its own image, `ghcr.io/alisonjenkins/pharos-aligner-rs` (`.#alignerRsOci`), which `aligner.shadow.image` runs. The primary keeps the Python `ghcr.io/alisonjenkins/pharos-aligner` until cutover, when `aligner.image` moves to the Rust image.
- New values:
  - `aligner.device` (default `auto`)
  - `aligner.gpu.enabled` (default `false`)
  - `aligner.gpu.devicePath` (default `/dev/dri`)
  - `aligner.gpu.resourceName`: a device-plugin resource such as `devic.es/dri`, requested when set
  - `aligner.cache.size` (default `1Gi`)
  - `aligner.cache.storageClass`
- The GPU is mounted as a `hostPath` at `devicePath`, as the server's `gpu.vaapi` does. The pod also gets `supplementalGroups` for the render group (gid from values).
- `/var/cache/pharos-aligner` is a PersistentVolumeClaim.
- The probes stay; the readiness probe passes only after warm-up. Warm-up runs before the port binds, so both Deployments also get a `/healthz` startup probe (every 10 s, 60 failures: 10 minutes), which holds off the liveness probe until the first answer.
- A second Deployment, `aligner-shadow`, renders when `aligner.shadow.enabled`. It runs the Rust image beside the Python one during the shadow week, and the server gets `aligner_shadow_url`.

## Server change (shadow only)

- `[lyrics].aligner_shadow_url` / `PHAROS_LYRICS_ALIGNER_SHADOW_URL`.
- After `process_job` stores an answer, a background task sends the same request to the shadow and compares the whole result: `offset_ms`, the line and word counts, and each word's `at` and `start_ms`. Only `aligner_version` is expected to differ, and it is left out of the equality check; but a shadow answer whose `aligner_version` is not 3 is counted as `outcome=error`, never as a match, so a shadow URL pointed at the Python service cannot pass the gate. It logs one line and bumps `pharos_lyrics_shadow_total{outcome}`, then drops the answer.
- At most one shadow request is in flight at a time, with the same 600 s timeout.

## Testing

- **Unit and parity:** fixtures generated from the Python code and torchaudio with `tests/fixtures/gen_fixtures.py` (slice 1 pattern), all synthetic, no media.
  - Slice 2 adds windows, probes, offsets and request parsing.
  - Slice 3 adds forward-pass emissions on seeded synthetic waveforms (noise, tones, chirps). The tolerance is max abs log-prob diff 2e-2, and CTC paths must come out identical on those emissions.
- **Forward tests:** they need `weights.bin`. They run under `nix flake check` from the converted-weights derivation, and are skipped with a printed reason when `PHAROS_ALIGNER_WEIGHTS` is unset.
- **Real songs:** checked locally against stored torch emissions (slice 1 recipe), never committed. In production, the shadow does the check.
- **Service:** an actix test server with a stub decoder and model, mirroring `aligner/tests/test_server.py`, covering the error statuses and the one-at-a-time lock.

## Risks

- Hand-written Burn ops miss parity. Mitigation: burn-onnx fallback (open question 2).
- Cold Vulkan shapes cost 13-16 s each. Mitigation: the persistent cache plus warm-up. Slice 5 measures this on the node.
- RADV on RDNA4 in the node's Mesa version is not yet proven inside the pod. Mitigation: the CPU path is always there, and `auto` falls back.
- The image grows by the Vulkan stack (Mesa) and loses torch, so it shrinks overall. Measured in slice 6.
