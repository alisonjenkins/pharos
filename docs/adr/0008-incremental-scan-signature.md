# ADR-0008: Incremental scan by (mtime, size) signature

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

The library scan walks the media roots and probes each file. On a ~12k-item
library over NFS, probing every file on every scan (boot, periodic, watcher-
triggered) is far too expensive to run repeatedly. Most files never change
between scans.

## Decision

The scanner (`pharos-scanner`, `FsScanner`) is **incremental**: for each file it
stats `(mtime, size)` and compares against the persisted per-item scan-state
signature. If unchanged, the expensive probe is **skipped** — the row is only
re-stamped (`mark_seen`) to keep the mark-and-sweep deletion pass current.
Content-fingerprinting distinguishes a moved/renamed file (same bytes, new path)
from a genuinely new one. Deletion is reconciled by sweeping rows not seen in the
current scan run.

A **`--force`** escape hatch bypasses the signature skip and re-probes every
file. It is exposed two ways:

- CLI: `pharos scan --force` (one-shot).
- HTTP: `POST /Library/Refresh?force=true` (admin, in-process) — the reliable
  path in production, since it runs on the server's own DB pool and survives
  client disconnect, unlike a `kubectl exec` of the CLI.

## Consequences

- A second (unchanged) scan probes zero files — cheap enough to run on every
  boot + periodically.
- **A probe-schema change is invisible to the incremental scan:** if pharos
  starts extracting a new field (e.g. embedded-font `MediaAttachments`), existing
  files are byte-identical, so their `(mtime, size)` is unchanged and they are
  never re-probed. `--force` is the required backfill trigger after such a
  change.
- The signature is `(mtime, size)`, not a content hash, so a rewrite that
  preserves both would be missed — an accepted trade for stat-only cost. `--force`
  covers the rare case.

## References

- `crates/pharos-scanner/src/fs.rs`
- `crates/pharos-server/src/api/jellyfin/admin.rs` (`library_refresh`), `cli.rs`
