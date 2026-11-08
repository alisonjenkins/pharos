# ADR-0074: The server allocates through jemalloc

- **Status:** Accepted
- **Date:** 2026-11-08T11:30:00Z
- **Deciders:** Alison Jenkins

## Context

Prod's pharos process grew steadily between deploys with nothing playing,
for example from 0.5 GiB to 3.4 GiB over 13 hours. On 2026-11-08 the live
process held 1.38 GiB of anonymous memory, of which about 0.9 GiB sat in
31 mappings of 16 to 64 MiB. That size is the signature of glibc malloc's
per-thread arenas.

Reproduced locally (release build, Postgres, 15.2k generated items, 3
minutes of playback, browsing and image requests):

| Allocator | RSS idle before | RSS peak | RSS 60 s after load |
|---|---|---|---|
| glibc (default) | 227 MiB | 1.80 GiB | 1.80 GiB |
| glibc, `MALLOC_ARENA_MAX=2` | 186 MiB | 471 MiB | 405 MiB |
| mimalloc 3.3 (preload) | 419 MiB | 1.40 GiB | 656 MiB |
| jemalloc 5.3, default decay (preload) | 250 MiB | 578 MiB | 397 MiB |
| jemalloc 5.3, background thread, 1 s dirty decay, 0 muzzy (preload) | 238 MiB | 506 MiB | 334 MiB |
| the same, built in (this decision) | 236 MiB | 519 MiB | 325 MiB |

heaptrack on the glibc run put the peak live heap at 285 MiB against
1.96 GiB RSS (heaptrack's own overhead included), so most of the RSS was
not live data. glibc kept freed memory in up to one arena per thread, and
the blocking pool grows from 35 to about 140 threads under load.

Latency (oha, 400 requests, concurrency 8, same build):

| Route | glibc | arena cap 2 | jemalloc |
|---|---|---|---|
| `Items/Latest` | 147 rps, p50 48 ms | 40 rps, p50 189 ms | 161 rps, p50 42 ms |
| `Items` paging | 1225 rps, p50 6 ms | 844 rps, p50 9 ms | 1250–1306 rps, p50 5–6 ms |

## Decision

- `pharos` (the server binary) sets `tikv_jemallocator::Jemalloc` as its
  `#[global_allocator]` on Linux.
- It is built with `unprefixed_malloc_on_supported_platforms`, so jemalloc
  also provides `malloc`/`free` for the in-process libav pool and every other
  C library in the process. Two allocators in one process would leave the
  libav half on glibc arenas.
- The binary exports `malloc_conf` =
  `background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0`. Freed pages
  go back to the kernel about a second after a burst, not at the next
  allocation. `MALLOC_CONF` in the environment still overrides it for
  diagnosis.
- `/metrics` exports `pharos_allocator_bytes{kind}` (`allocated`, `active`,
  `resident`, `mapped`, `retained`). A leak then shows as `allocated`
  rising, and allocator retention as the gap between `allocated` and
  `resident`.
- `transcode-worker` and other binaries keep the system allocator. They are
  short-lived and each holds about 4 MiB of anonymous memory.

## Consequences

- About 1.5 GiB less RSS after load in the local reproduction. In prod
  this should end the climb between deploys. Check it against
  `process_resident_memory_bytes` and `pharos_allocator_bytes` after rollout.
- One more C dependency built from source (jemalloc 5.3.1 through
  `tikv-jemalloc-sys`). In the dev profile it is pinned to `opt-level = 3`,
  because jemalloc's `configure` fails at `-O0` under the nix shell's
  `_FORTIFY_SOURCE` hardening.
- Not Linux: macOS and Windows builds keep the system allocator.
- Rejected: `MALLOC_ARENA_MAX=2` saves nearly as much memory, but it made
  the allocation-heavy `Latest` route 3.7× slower through arena lock
  contention. mimalloc kept more than twice jemalloc's RSS after load.
