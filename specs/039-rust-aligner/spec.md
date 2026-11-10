# Feature specification: a Rust aligner service

**Feature Branch**: `feat/rust-aligner-ctc` (slice 1), later slices stacked on it
**Created**: 2026-11-10T11:55:00Z
**Status**: Draft. Slice 1 (CTC, text, tokens) built on `feat/rust-aligner-ctc`, not merged.
**Scope**: Replace the Python aligner service (`aligner/`, ADR-0066) with one Rust service. It answers the same HTTP contract and runs the MMS_FA forward pass itself: on the GPU through Burn's wgpu backend on Vulkan in f32, or on the CPU through a hand-written OpenBLAS forward pass. The pharos server, its align queue and the stored word times are unchanged, apart from an optional shadow comparison used for the cutover.
**Decision record**: ADR-0116. **Bugs reserved**: B1290-B1299. **Verification**: V1250-V1269.

---

## Problem statement

Karaoke word times come from `pharos-aligner`, a Python service on torch and torchaudio.

- It is slow on the shared node. torch's forward pass is 99% of a song's time. With torch's default thread count, one song took 330-490 s on the 44-vCPU node next to the CI runners (capped at 8 threads since B1166: about 30 s a song).
- It cannot use the node's GPU. The cluster GPU is an AMD RX 9070 (RDNA4), reachable through Vulkan; the image has no ROCm, and ROCm is not wanted.
- It holds about 2.6 GiB resident for one model, and RAM is scarce on the node.
- It is the only Python service, with its own image, toolchain and dependency churn (torch, torchaudio, OpenTelemetry).

Measured on 2026-11-10 on a Ryzen 7950X, 8 threads, against torch on the same windows:

| Path | Speed vs torch | Word Δ | RSS |
|---|---|---|---|
| Burn wgpu, Vulkan f32, RX 9070 XT | 8-10 s a song steady; first use of a window length 13-16 s | 0 | (not measured; VRAM) |
| Hand-written CPU forward, threads pinned one per physical core in one L3 | 1.09-1.12x (one window per pass), 1.19-1.22x (about 3 windows per pass) | 0 | 2.0-2.4 GiB |
| Hand-written CPU forward, unpinned | about 1.0x | 0 | 2.0-2.4 GiB |
| Burn CPU (patched flex) | 0.67-0.77x | 0 | 3.1 GiB |
| Burn fp16 on Vulkan | about 3x faster | up to 500 ms | rejected |

Pinning helps torch as much as it helps Rust (song A: 24 s unpinned, 16 s pinned). The Rust CPU path wins on RAM and by removing glue, not on GEMM speed: both call the same OpenBLAS.

## User stories

1. As a listener, karaoke lines light up on the sung word as they do today. The new service's word times equal the Python service's on the same song, to the millisecond, before it replaces it.
2. As a listener, a song's words arrive sooner after I first open its karaoke view, because alignment runs on the GPU when there is one.
3. As the operator, I can see from one log line or span which device aligned a song and why that device was chosen, and from a query how often it fell back to the CPU.
4. As the operator, the service runs with no GPU at all (dev machines, a node without `/dev/dri`) and gives the same answers, more slowly.
5. As the operator, the service's resident memory stays at or below the Python service's, with a cap on how much audio one forward pass may hold.

## Requirements

### Contract (unchanged)

- R1. `POST /align` takes the same JSON body (`path` under the media root, `lines` with `text`, `start_ms`, `end_ms`) and returns the same body: `lines[].words[]` with `at` and `start_ms`, `offset_ms` and `aligner_version`. Error statuses and messages stay: 400 for a bad body or path, 404 for a missing file, 413 over 1 MiB, 422 when ffmpeg fails, 500 otherwise.
- R2. `GET /healthz` answers `{"ok": true}` while an alignment runs.
- R3. The same environment variables keep their meaning: `PHAROS_ALIGNER_MEDIA_ROOT`, `PHAROS_ALIGNER_LISTEN` (default `0.0.0.0:8790`), `PHAROS_ALIGNER_OTLP_ENDPOINT`, `PHAROS_ALIGNER_THREADS` (default 8), `PHAROS_ALIGNER_IDLE_UNLOAD_SECS` (default 600, 0 keeps the model loaded). One is new: `PHAROS_ALIGNER_FFMPEG` names the ffmpeg to run (default `ffmpeg` on `PATH`), so the image can pin its own.
- R4. `aligner_version` becomes 3, so each answer shows which service made it. The server does not store the version; it checks it on receipt. The server's `MIN_ALIGNER_VERSION` stays 2: answers from the Python service stay valid.
- R5. One alignment at a time, as today. A request that waits for another is visible as its own span.
- R6. Spans join the server's trace by `traceparent` (ADR-0105) under the same names: `aligner.align`, `aligner.wait`, `aligner.decode`, `aligner.model`, `aligner.forward`, `aligner.ctc`. The song's log line keeps its fields and phase totals.

### Pipeline parity

- R7. Text, tokens, the offset estimate (`offset.py`), the line windows (`plan.py`, including the lead clamp to the previous line's aligned end, B1165), CTC and the frame-to-ms conversion match the Python code exactly. Slice 1 proved this for text, tokens, CTC and ms.
- R8. Audio is decoded per window by spawning `ffmpeg` with the Python service's arguments (`-ss`, `-t`, `-ac 1 -ar 16000 -f f32le`). Decoding the song once and slicing it moved words by up to 542 ms, so it is not used.
- R9. The waveform is layer-normalised over the window (f64 mean and variance, eps 1e-5), the model's 28 logits get log_softmax, and a zero column is appended for the star, as torchaudio's MMS_FA bundle does.

### Devices

- R10. `PHAROS_ALIGNER_DEVICE` is `auto` (default), `vulkan` or `cpu`.
  - `auto` uses Vulkan when wgpu finds a non-CPU Vulkan adapter and the warm-up forward succeeds. It uses the CPU otherwise.
  - `vulkan` refuses to start without such an adapter; `cpu` never opens a GPU.
  - Software Vulkan (llvmpipe, lavapipe) counts as no adapter.
- R11. The chosen device, the reason and the adapter name are logged once at start, set on every `aligner.forward` span (`aligner.device`, `aligner.device_reason`) and carried on the song's log line. The start reasons are `forced_cpu`, `forced_vulkan`, `adapter_found`, `no_adapter`, `software_adapter_only` and `warmup_failed:<error>`. A song that left the GPU carries `device=cpu` with `gpu_error` (a GPU error in this song) or `latched` (R12's latch is set); the CPU pass loaded behind the GPU tags its spans `gpu_fallback`.
- R12. A GPU error during a song (device lost, out of memory) aligns that song on the CPU. A panic in a GPU window counts as an error. The rest of the song stays on the CPU. The warning names the error, and `pharos_aligner_device_fallback_total{reason="gpu_error"}` counts it. After 3 such errors in 10 minutes, the service stays on the CPU until restart: it logs an error once, and counts every later song as `reason="latched"`. The CPU pass loads on the first fallback, not at start.
- R13. Vulkan runs f32 only. fp16 moved words by up to 500 ms.

### GPU warm-up and cache

- R14. cubecl's autotune and kernel cache live in `PHAROS_ALIGNER_CACHE_DIR` (default `/var/cache/pharos-aligner`). The service creates the directory. If it cannot, it warns `kernel_cache=off_unwritable:<path>: <error>` and compiles in memory. The chart backs it with a persistent volume, so a restart does not pay the first-use cost again.
- R15. Windows are zero-padded to fixed lengths: 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48 and 64 s, then multiples of 32 s. The padding is masked out of attention and cut off the logits, so words match the unpadded pass. At start, before `/healthz` reports ready, the service runs one synthetic window at every length up to 64 s, logs each one's time, and gives the warmed model to the first song.

### CPU path

- R16. On the CPU, the forward pass is the hand-written one: channels-last activations and OpenBLAS sgemm for the linears and convolutions, with fused QKV, bias and residual folded into the GEMMs, and vectorised GELU, softmax and LayerNorm.
- R17. One copy of the weights. A converted `weights.bin` (f32, little-endian) and its index are mmapped; only fused QKV and reordered conv weights are copied.
- R18. Thread pinning, `PHAROS_ALIGNER_PIN`, is `auto` (default), `on` or `off`. `on` pins worker threads one per physical core, filling one L3 domain first, within the pod's allowed CPUs. `auto` pins only when sysfs shows more than one L3 domain or SMT siblings among the allowed CPUs. On the production node it does not: that node is a VM whose 44 vCPUs sit in one L3 domain with no SMT, and the hypervisor does not pin them (host EPYC 7543P, 4 CCDs). There, pinning cannot target a CCD, and the shadow slice measures whether it helps at all.
- R19. `PHAROS_ALIGNER_BATCH_SAMPLES` (default 1,000,000, about 62 s of audio) caps the audio one forward pass holds. Independent windows are stacked up to the cap; a window longer than the cap runs alone. Only the offset probes are independent: each line's window depends on the previous line's aligned end, so line windows run one at a time.
- R20. Peak RSS over a song stays at or below 2.6 GiB with the default cap.

### Rollout

- R21. The pharos server gains an optional `[lyrics].aligner_shadow_url`. When it is set, each finished alignment is also sent to the shadow aligner after the primary answer is stored, at most one at a time. The two answers are compared in full (`offset_ms`, line and word counts, and each word's `at` and `start_ms`), and the result is logged and counted; the shadow answer is never stored or served.
- R22. The cutover points `[lyrics].aligner_url` at the Rust service after the shadow shows no word difference over at least 50 songs. A later slice removes the Python service, its image and its flake outputs.

## Observability (named signals)

| Question | Signal |
|---|---|
| Which device aligned this song, and why? | Log line `path=... device=vulkan device_reason=adapter_found adapter="..." duration_ms=...`; span attributes `aligner.device`, `aligner.device_reason` |
| How often do we fall back to the CPU? | `pharos_aligner_device_fallback_total{reason}` on the aligner's `/metrics`; warning `GPU forward failed; this song continues on the CPU error=... latched=...`; the song line's `device=cpu device_reason=gpu_error\|latched` |
| How long does the forward pass take per device? | `pharos_aligner_forward_seconds{device}` histogram; `aligner.forward` span duration |
| Did warm-up hit the cache? | Start logs `GPU kernel cache kernel_cache=<dir or off_...>` and `GPU warm-up buckets=12 total_ms=... per_bucket="1s=...ms ..."`. On the desktop RX 9070 XT, an empty cache takes about 35 s and a reused one about 9.6 s |
| Does the Rust service agree with Python? | Server log `lyrics shadow item=<id> words=N differing=M max_delta_ms=D`; `pharos_lyrics_shadow_total{outcome=match\|differ\|error}` |
| Is memory within budget? | `process_resident_memory_bytes` on `/metrics`; `aligner.rss_mib` on the song's log line |

LogQL: `{namespace="pharos", app_kubernetes_io_component="aligner"} |= "device="`. PromQL: `sum by (reason) (increase(pharos_aligner_device_fallback_total[1d]))`, `sum by (outcome) (increase(pharos_lyrics_shadow_total[7d]))`.

## Verification

| Id | Statement | How |
|---|---|---|
| V1250 | `words()` gives the Python tokens and code-point offsets for every code point Python assigns, its whitespace set and mixed strings | `it_parity::text` against `text.json` |
| V1251 | CTC paths and log scores equal torchaudio's bit for bit; span scores within 1e-6; infeasible targets are an error carrying frames, targets and repeats | `it_parity::ctc` against `ctc.json` |
| V1252 | A line's word ms equal model.py's for the same emission, including lead stars, short windows and the repeat bound | `it_parity::line` against `line.json` |
| V1253 | Line windows, stamps, request parsing and path resolution equal plan.py's | parity fixtures from plan.py |
| V1254 | Probe choice, offset estimate and shift equal offset.py's | parity fixtures from offset.py |
| V1255 | The HTTP contract matches the Python service: bodies, statuses, error messages, `aligner_version` 3 | service tests mirroring `test_server.py` |
| V1256 | Spans keep the ADR-0105 names and attributes, plus `aligner.device` and `aligner.device_reason` | span test, disarmed once |
| V1257 | The CPU forward pass's log-probs are within 2e-2 of torch on synthetic waveforms, and CTC paths are identical | emission fixtures, weights from the flake |
| V1258 | Without AVX2 the service refuses to start, naming the feature | unit test on the feature check |
| V1259 | Pinning picks one thread per physical core in the largest L3 domain, and `auto` does nothing on a single-L3, no-SMT CPU set | synthetic sysfs trees |
| V1260 | Peak RSS stays at or below 2.6 GiB with the default batch cap | RSS test on synthetic songs |
| V1261 | The Vulkan forward pass meets V1257's bar | the same fixtures on a Vulkan adapter (skipped without one, with a printed reason) |
| V1262 | The device choice follows R10, and every reason in R11 is produced by its case; software adapters are refused | fake adapter lists |
| V1263 | A GPU error aligns the song on the CPU and counts it; 3 in 10 minutes latch the CPU until restart | fault-injection test |
| V1264 | Warm-up runs before readiness and logs its lengths, time and cache use | start-up test |
| V1265 | The chart renders the GPU mount, the render group, the cache PVC and the shadow Deployment only when asked | `helm template` tests |
| V1266 | Readiness fails until warm-up is done | probe test |
| V1267 | The shadow comparison never stores or serves the shadow answer, keeps one in flight, and logs and counts each outcome | server unit tests |
| V1268 | After cutover, every answer the service returns carries `aligner_version` 3, and words cached from version-2 answers are still served | server test plus a query after the deploy |
| V1269 | No Python aligner remains in the flake, the chart or CI | `nix flake show` / grep check in CI |

## Out of scope

- Other alignment models, other languages' tokenisers, a LiteLLM-backed path (queued separately, never the default).
- Changing how the server queues, stores or serves word times.
- Host-side vCPU pinning on the hypervisor (a nix-config change, raised separately if the shadow slice shows pinning matters).

## Open questions

1. The window lengths for warm-up (R15): one per 0.5 s bucket from 1 to 20 s, or the lengths seen in a week of real requests. Slice 5 measures the cold cost per length on the RX 9070 and picks.
2. Whether the Burn model is built from the same `weights.bin` with hand-written Burn ops (one weights artifact, parity re-proven), or generated by burn-onnx from an ONNX export (proven word Δ 0, but a second artifact). The plan prefers the first; slice 3 decides on the parity result.
