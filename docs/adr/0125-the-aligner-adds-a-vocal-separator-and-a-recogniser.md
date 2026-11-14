# ADR-0125: the aligner adds a vocal separator (htdemucs) and a recogniser (Whisper large-v3-turbo)

- **Status:** Proposed
- **Date:** 2026-11-14T07:55:00Z
- **Deciders:** Alison (approach chosen 2026-11-14T06:15Z); runtime choice open until the spikes in plan slices 2 and 3

## Context

ADR-0124 needs separated vocals and a recogniser with word times. The aligner is Rust only (ADR-0116, no Python). No maintained Rust port of either model fits: the Burn Whisper ports are tiny-model only without word times, candle has no Vulkan, and Demucs exists in C++ (demucs.cpp) and as ONNX exports.

Measured in Python on the desktop GPU: htdemucs 6-16 s a song, +1.3 GB VRAM; Whisper large-v3-turbo 6-37 s, +4.1 GB (fp16); large-v3 was no better on the set at twice the memory.

## Decision

We will use htdemucs and Whisper large-v3-turbo, loaded in stages and released between stages so one song holds under 6 GiB of GPU memory, with a CPU path for both.

- Recogniser runtime: whisper.cpp through `whisper-rs` with its Vulkan backend, if a one-day spike shows word times within 100 ms of the Python prototype and no new hallucination classes. Otherwise a hand port in Burn (Vulkan f32), like the letter model.
- Separator runtime: the htdemucs ONNX export imported into Burn, if a half-day spike matches the Python stems. Otherwise a hand port in Burn from demucs.cpp.
- Weights are fetched and converted in the flake, as the letter model's are; the image has no Python.

## Consequences

- Two more model weights in the image and on disk, and up to ~6 GiB GPU memory per song.
- If whisper.cpp is chosen, the build gains C++ and the process gains a second GPU runtime beside Burn.
- ROCm attention kernels gave wrong stems in Python on this card; the Rust paths must be checked against CPU output.

## Alternatives considered

- Whisper large-v3: no more lines anchored on the set, twice the memory.
- No separation: fewer lines anchored on the mix (88 vs 97 of 110) and worse refinement.
- Python sidecar: rejected; the aligner is Rust only.

## References

- specs/042-karaoke-recogniser-alignment/plan.md (slices 2 and 3)
- ADR-0116, ADR-0124
