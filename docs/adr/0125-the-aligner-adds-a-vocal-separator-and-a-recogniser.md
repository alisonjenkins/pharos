# ADR-0125: the aligner adds a vocal separator (htdemucs) and a recogniser (Whisper large-v3-turbo)

- **Status:** Proposed
- **Date:** 2026-11-14T07:55:00Z
- **Deciders:** Alison (approach chosen 2026-11-14T06:15Z); runtimes chosen by the slice 2 and 3 spikes

## Context

ADR-0124 needs separated vocals and a recogniser with word times. The aligner is Rust only (ADR-0116, no Python). No maintained Rust port of either model fits: the Burn Whisper ports are tiny-model only without word times, candle has no Vulkan, and Demucs exists in C++ (demucs.cpp) and as ONNX exports.

Measured in Python on the desktop GPU: htdemucs 6-16 s a song, +1.3 GB VRAM; Whisper large-v3-turbo 6-37 s, +4.1 GB (fp16); large-v3 was no better on the set at twice the memory.

## Decision

We will use htdemucs and Whisper large-v3-turbo, loaded in stages and released between stages so one song holds under 6 GiB of GPU memory, with a CPU path for both.

- Recogniser runtime: whisper.cpp through `whisper-rs` with its Vulkan backend, chosen after a spike on the private set (2026-11-14): DTW word starts within 100 ms of the Python prototype for 81% of matched words (median 20 ms), final placements within 300 ms on 91% of lines, about 4x faster, RSS 0.3 GB against 4.0 GB, VRAM +2.3 GB. Settings it needs: DTW word times (a word starts at the previous token's DTW time), `n_max_text_ctx = 0` (text otherwise carries between windows), flash attention off, whisper.cpp's own seeking (no external windows). One song lost whole windows to "Thank you." decodes; the anchor-share gate keeps it at today's answer, and the cause must be found before cutover. A hand port in Burn stays the fallback.
- Separator runtime: the htdemucs core (without STFT/ISTFT) exported to ONNX opset 17, simplified with onnxsim, and generated into Burn code with burn-onnx, run on Vulkan with the cubecl kernel cache. The spike (2026-11-14) matched ONNX Runtime within 1.8e-4 at 0.32 s per 7.8 s segment (about 15 s a song), +0.95 GiB VRAM warm. STFT/ISTFT are written in Rust outside the graph. Burn's CPU backend is 7x slower than torch, so without a GPU the recogniser runs on the mix instead (fewer lines anchored: 88 vs 97 of 110 on the prototype set).
- Weights are fetched and converted in the flake, as the letter model's are; the image has no Python.

## Consequences

- Two more model weights in the image and on disk, and up to ~6 GiB GPU memory per song.
- The build gains C++ and a second GPU runtime beside Burn: cmake, shaderc, vulkan-headers and vulkan-loader in the devShell and the package, a ~74 s first compile, a ~40 MB binary increase and a 1.6 GB ggml model file.
- ROCm attention kernels gave wrong stems in Python on this card; the Rust paths must be checked against CPU output.

## Alternatives considered

- Whisper large-v3: no more lines anchored on the set, twice the memory.
- No separation: fewer lines anchored on the mix (88 vs 97 of 110) and worse refinement.
- Python sidecar: rejected; the aligner is Rust only.

## References

- specs/042-karaoke-recogniser-alignment/plan.md (slices 2 and 3)
- ADR-0116, ADR-0124
