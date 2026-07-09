# ADR-0004: libav in-process default backend + crash-isolated worker pool

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos does two very different kinds of ffmpeg work:

1. **High-frequency "tiny ops"** — probe a file, extract one poster/thumb frame,
   build trickplay sprite sheets, convert a subtitle, compute a waveform. These
   run constantly during scans and browsing.
2. **Video-segment / live transcode** — long-running encodes where the CPU/GPU
   encode time dwarfs everything else.

Forking `ffmpeg`/`ffprobe` per tiny op pays a fork/exec + process-startup cost on
the hottest path (a 12k-item scan is 12k `ffprobe` spawns). But calling libav
in-process couples a C library that segfaults on malformed input to the server
process — invariant **V6** says a bad file must never take the server down.

## Decision

Two interchangeable backends, selected by Cargo feature; **`ffmpeg-lib`
(libav via `ffmpeg-the-third`) is the default**, `ffmpeg-spawn` is the
alternative (`--no-default-features --features backend-spawn`).

The libav backend does **not** run in the server process directly. Tiny ops are
serviced by a pool of persistent, crash-isolated `transcode-worker`
subprocesses (`LibavWorkerPool`): the fork/exec is paid once per worker and
amortised across many ops; a libav segfault kills only that worker (the pool
sees EOF and respawns), never the server. Video-segment / live transcode
**always** uses the spawn worker regardless of backend — encode time dominates,
so fork/exec is noise, and the GPU/CPU scheduler already load-balances it.

## Consequences

- Tiny-op throughput is high (resident workers, no per-op spawn) while the V6
  crash-isolation guarantee holds.
- The default build depends on the `ffmpeg-the-third` FFI crate → needs libav
  headers + bindgen + `LIBCLANG_PATH` at build time. The Nix devShell provides
  these (ADR-0009).
- The OCI image must build via `buildRustPackage` (cargo), **not** crate2nix:
  pinned nixpkgs `buildRustCrate` mishandles the crate's modern `cargo::` cfg
  syntax and compiles the wrong libav API (see ADR-0009).
- The distroless runtime image ships **no `ffprobe` binary** — anything that
  reached for `FfmpegProber` at runtime silently failed until it was switched to
  the libav prober. Runtime code must use the libav path.
- Worker ops carry timeouts: 60s for tiny ops, 900s for whole-file "heavy" ops
  (trickplay / waveform) that legitimately walk a long file over NFS.

## References

- `crates/pharos-transcode/` (`worker/libav_pool.rs`, `bin/transcode_worker.rs`)
- `CLAUDE.md` §Transcode; memory `project_ffmpeg_lib_default`
