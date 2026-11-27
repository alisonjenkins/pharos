# ADR-0159: Keyed cache paths are changed under a cross-process lock

- **Status:** Accepted
- **Date:** 2026-11-27T04:20:00Z
- **Deciders:** Alison Jenkins

## Context

A cached HLS segment is a pair: the bytes at the keyed path and the
provenance record beside them, which says whether the bytes are a
synthesized filler (B1539/V1540). A record that describes the wrong bytes
lets a filler be served `immutable`. A replacement that loses its record
reads as unconfirmed and is never retried.

The in-flight registry serialises work on one key, but only inside one
process. During a rolling update, two server pods share one cache volume.
Publication, retirement, eviction and purge of the same keyed path could
therefore interleave across pods.

## Decision

We will change keyed cache paths only while holding an advisory
`flock(LOCK_EX)`.

- **Lock files:** there are 64 stripe files under `<cache root>/.locks/NN.lock`. A segment path picks its stripe by hash, and a sidecar maps to its segment's stripe. The files are created on demand and never deleted.
- **Held across:**
  - the occupied check, record write and rename of publication;
  - retirement and defect quarantine;
  - purge;
  - eviction of a segment together with its record.
- **Recheck:** eviction rechecks, under the lock, that the unit is still the one it scanned.
- **Acquisition:** an async try-lock loop (10 ms step, 10 s deadline). Critical sections hold filesystem operations only, never an encode.
- **Fail closed:** if the lock cannot be taken, the keyed path is left alone. Fresh bytes are served `Transient` (`no-store`, no ETag), and `pharos_segment_publish_lock_failed_total` counts it.
- **Readers take no lock.** They read the bytes, then the record, then re-stat the file. A torn pair reads as unconfirmed (`no-store`).

## Consequences

- Two pods sharing the cache cannot leave a record that describes the wrong bytes.
- The cache volume needs a filesystem with working `flock`. Local disks and the current PVC have it; some network filesystems do not.
- Unrelated segments that share a stripe can wait briefly on each other.
- `pharos-cache` gains a direct `libc` dependency. It was already in the workspace.
- The lock is not reentrant, so code must never take it while holding it.

## Alternatives considered

- **A lock file in each media directory.** Rejected: a lock file that is never deleted stops eviction from pruning empty directories.
- **Locking the hit path too.** Rejected: an open plus `flock` on every hit, when the re-stat check already detects a torn pair.
- **Publishing the segment and its record together by swapping a directory.** Not evaluated.
- **Declaring a shared cache mount unsupported.** Rejected: the deployment runs rolling updates on a shared volume.

## References

- B1539 and V1540 in `specs/001-pharos-baseline/`.
- `crates/pharos-cache/src/hls_cache.rs` (`publish_lock`).
- The precedent for `libc::flock`: `crates/pharos-server/src/opensubtitles/store.rs`.
