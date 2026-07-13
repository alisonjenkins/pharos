# ADR-0007: Trickplay pre-generation with playback-yield gate

- **Status:** Accepted — gate mechanism superseded by
  [ADR-0017](0017-adaptive-background-io-gate.md) (adaptive shared semaphore
  replaces the binary quiet gate; pre-generation model + seed bypass stand)
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

Trickplay (scrub-preview thumbnail sprites) requires decoding frames at intervals
across a whole file — a heavy, whole-file pass. Generating it on-demand when the
client requests a tile would make the scrub bar block on minutes of decode.
Jellyfin clients expect tiles to already exist and 404 gracefully otherwise.

At the same time, a whole-file decode competing with a live VP9 transcode on the
same box causes the viewer to buffer — the exact problem the playback-priority
work fought.

## Decision

Trickplay (and warmed subtitle extraction) is **pre-generated out of band** by a
background task (`trickplay_backfill.rs`) into a persistent cache
(`TrickplayCache` on the cache PVC); the HTTP tile route serves from cache only
and 404s when absent. The backfill has three tiers:

1. **Priority** — actively-watched items, expanded to the whole series.
2. **General sweep** — newest-first across the library.
3. Idle until the next pass or a new play event.

To protect live playback, generation **yields** via a playback-quiet gate
(`await_gate`): it parks until streaming has been idle 30s before launching a
decode — **except** the priority-tier *seed* (the item being watched right now),
which **bypasses the gate**. Without that exception the previews for the very
item you are scrubbing never generate during the session that wants them (the
gate is never satisfied while you watch, and there is no on-demand fallback).
Series siblings and the bulk sweep stay gated, so only the single watched item
competes with its own stream.

Client wiring: the `BaseItemDto.Trickplay` manifest must be the **double-nested**
`{ mediaSourceId: { width: TileInfo } }` shape or jellyfin-web never requests a
tile.

## Consequences

- Scrubbing a *previously-watched* (cached) item is instant.
- Scrubbing a *fresh* item shows a black preview until its seed generation
  completes (bounded, one whole-file pass), then works for the rest of the
  session and forever after (persistent cache).
- The seed bypass reintroduces bounded contention (one item's decode) with the
  live stream; if that proves to cause buffering, the mitigation is rate-limiting
  the seed decode, not re-gating it.
- Whole-file trickplay ops use the 900s "heavy op" worker timeout (ADR-0004).

## References

- `crates/pharos-server/src/trickplay_backfill.rs`
- `crates/pharos-jellyfin-api/src/dto.rs` (`with_trickplay`, nested manifest)
