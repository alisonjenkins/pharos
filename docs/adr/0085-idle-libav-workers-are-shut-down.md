# ADR-0085: Idle libav workers are shut down

- **Status:** Accepted
- **Date:** 2026-11-09T14:40:00Z
- **Deciders:** Alison Jenkins

## Context

The in-process libav ops (probe, image, trickplay, subtitle, waveform,
fingerprint) run in a pool of `transcode-worker` processes (P48). Workers
were spawned on demand and then kept for the life of the server. The server
builds more than one pool: one shared by the caches, one per scanner library
root, and one per admin-refresh group, each capped at `min(cpus, 8)`. On
2026-11-09 the live pod held 12 workers, all idle, at ~125–140 MiB anon,
next to a server heap of ~440 MiB. RAM on the node is short, and Alison asked
for memory over the cold-start cost.

A fresh worker costs one fork/exec, a handshake and libav init. Measured on
2026-11-09 (release, 1080p H.264 clip): the first probe takes 38 ms against
4.7 ms warm, and the first image extract 59 ms against 18 ms.

## Decision

We will shut down a pooled worker once it has sat idle for the pool's idle
timeout, and spawn a fresh one on the next op. Pools built with
`LibavWorkerPool::with_discovered_bin` (every pool `serve` builds) default
to 60 s. `PHAROS_LIBAV_WORKER_IDLE_SECS` overrides it, and `0` keeps workers
resident, as before. A reaper task per pool runs only while idle workers
exist: it sends `Shutdown`, then drops the worker. Every spawn and exit is
counted in `pharos_libav_worker_lifecycle_total{event,reason}`.

60 s is long enough that a scan, sweep or burst of poster requests, which
issue ops back to back, keeps its workers. It is short enough that workers
from a finished scan are gone within a minute.

## Consequences

- Idle anon memory falls by the workers' share, ~130 MiB on the live pod,
  and the workers' count now follows load.
- The first libav op after a minute of quiet pays ~34–40 ms. Cached images
  and probes are unaffected; only cache misses wait.
- A busy pool whose gaps exceed 60 s respawns workers repeatedly. The
  lifecycle counter shows it (`spawned` close to `exited/idle_timeout`); raise
  the timeout if that happens.
- The pools are still separate, so a scan can briefly run up to 8 workers
  beside the shared pool's. Merging them into one pool is a separate
  decision.

## Alternatives considered

- **Keep workers resident (status quo):** no cold start, ~130 MiB held
  idle.
- **Keep one warm worker per pool:** the first op stays fast, but four or more
  pools would keep four or more workers, most of the saving lost.
- **A shorter timeout (5–10 s):** gaps between user requests are often
  longer than that, so most cache misses would pay the spawn.

## References

- B913, V874 in `specs/001-pharos-baseline/`.
- `crates/pharos-transcode/src/worker/libav_pool.rs`.
- ADR-0074 (jemalloc in the server); B911 (jemalloc in the worker).
