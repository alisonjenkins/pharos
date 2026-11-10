# ADR-0116: A Rust service replaces the Python aligner, on Vulkan or the CPU

- **Status:** Proposed
- **Date:** 2026-11-10T12:00:00Z
- **Deciders:** Alison Jenkins

## Context

Karaoke word times come from `pharos-aligner` (ADR-0066), a Python service that runs torchaudio's MMS_FA model on torch.

- The forward pass is 99% of a song's time. On the shared 44-vCPU node, a song takes about 30 s at the 8-thread cap (B1166) and took up to 500 s without it.
- The node has an AMD RX 9070, which the service cannot use: there is no ROCm in the image, and ROCm is not wanted.
- The service holds about 2.6 GiB resident. It is also the project's only Python service, with its own toolchain and dependency churn.

Spikes on 2026-11-10 (spec 039):
- Burn's wgpu backend on Vulkan in f32 gives the same words as torch (Δ 0), at 8-10 s a song once warm. fp16 moved words by up to 500 ms.
- A hand-written Rust CPU forward pass on the same OpenBLAS gives the same words in 2.0-2.4 GiB. It is 1.09-1.22x torch's speed with both pinned one thread per core in one L3 domain, and about equal unpinned.
- A Rust port of the CTC alignment, text and token handling matches torchaudio bit for bit, and matched 658 real words with Δ 0.

## Decision

We will replace the Python aligner with one Rust service, built from the new crate `crates/pharos-align` with the binary `pharos-aligner`. It has no Python.

1. **Same HTTP contract.**
   - `POST /align`, `GET /healthz`, the same JSON bodies, error statuses and environment variables.
   - `aligner_version` becomes 3. The server's minimum stays 2, so stored Python answers stay valid.
   - Spans keep the ADR-0105 names, and `/metrics` is added.
2. **Same pipeline.**
   - Text, tokens, offset probes, line windows (with the B1165 lead clamp), CTC and ms conversion are ports checked against the Python code by fixtures.
   - Audio is decoded per window by spawning `ffmpeg` with the Python arguments. Decoding once and slicing moved words.
3. **Device: `PHAROS_ALIGNER_DEVICE=auto|vulkan|cpu`, default `auto`.**
   - `auto` uses the first non-CPU Vulkan adapter (llvmpipe and lavapipe refused) if a warm-up forward succeeds on it, and the CPU otherwise.
   - The device, the reason (`forced_cpu`, `forced_vulkan`, `adapter_found`, `no_adapter`, `software_adapter_only`, `warmup_failed:<error>`) and the adapter name go on the start log, on every `aligner.forward` span and on the song's log line.
   - A GPU error during a song finishes that song on the CPU and is counted. Three in ten minutes keep the service on the CPU until restart.
4. **GPU path:** Burn `wgpu` on Vulkan, f32 only.
   - cubecl's autotune and kernel cache persist in `PHAROS_ALIGNER_CACHE_DIR`, on a PersistentVolumeClaim in the chart.
   - Warm-up forwards at a fixed set of window lengths run before readiness.
5. **CPU path:** the hand-written OpenBLAS forward pass. It needs AVX2 and FMA, and refuses to start without them.
   - One mmapped copy of converted f32 weights, shared with the GPU path's upload.
   - Pinning `auto|on|off` (default `auto`: pin one thread per physical core in the largest L3 domain only when the CPU set has more than one L3 domain or SMT siblings). On the production node, a VM with 44 unpinned vCPUs in one reported L3 domain, it does not pin. The shadow week measures whether `on` helps there.
   - A batch cap (`PHAROS_ALIGNER_BATCH_SAMPLES`, default 1,000,000) bounds the audio one forward holds.
6. **Chart:**
   - The aligner Deployment mounts `/dev/dri` as a hostPath when `aligner.gpu.enabled`, requests an optional device-plugin resource, and adds the render group.
   - It gets a cache PVC, and an optional `aligner-shadow` Deployment.
7. **Cutover:**
   - The server gains `[lyrics].aligner_shadow_url`. After each stored answer, it sends the same request to the shadow, compares word times and counts `pharos_lyrics_shadow_total{outcome}`, but never stores or serves the shadow answer.
   - `aligner_url` moves to the Rust service after at least 50 consecutive shadow `outcome=match` comparisons with no mismatch, each comparing the full answer, per-line `score` included, from an `aligner_version` 5 shadow. A mismatch starts the count again; errors do not count and must stay under 5% of the run's comparisons. Then `aligner/` and its flake outputs go.

## Consequences

- One language and toolchain for the whole server side. torch, torchaudio and the Python OpenTelemetry stack leave the image; Mesa's Vulkan driver and OpenBLAS come in.
- Alignment can use the GPU the node already has. On a machine without one, it runs on the CPU with the same answers, in less RAM than today.
- We own a forward pass for one model. A different model would need its own port or a generated graph; this decision does not cover that.
- The Burn model is written by hand from the same weights. If its parity misses, burn-onnx generation from an ONNX export is the fallback, at the cost of a second weights artifact.
- First-use Vulkan shapes cost 13-16 s each. The persistent cache and warm-up are the mitigation, and slice 5 measures them on the node.
- The pinning default is a measured guess on this node. Host-side vCPU pinning would be a separate nix-config decision.
- The image needs the converted weights from a build-time torch step. Torch stays a build input, not a runtime one.

## Alternatives considered

- **Keep Python, add a GPU.** ROCm torch is not wanted, and torch's Vulkan backend is not maintained. It also leaves the RAM and the Python stack.
- **Burn for the CPU path too (patched flex).** Word Δ 0, but 1.3-1.5x slower than torch and 3.1 GiB.
- **cubecl-cpu.** 54-118 s per 3.9 s window.
- **fp16 on Vulkan.** About 3x faster, but NaN in f16 attention, and words moved by up to 500 ms with f32 LayerNorm and attention.
- **Decode the song once and slice it.** Faster, but changed words by up to 542 ms.
- **ONNX Runtime.** Not measured. It would add a C++ runtime and a second model format, and gives no Vulkan path on AMD.

## References

- Spec 039 (`specs/039-rust-aligner/`), ADR-0066 (separate aligner service), ADR-0105 (aligner OTLP spans).
- B1165 (lead window), B1166 (thread cap).
- torchaudio v2.11.0 `src/libtorchaudio/forced_align/cpu/compute.cpp`.
