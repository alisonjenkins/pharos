# ADR-0027: One seek model for DirectPlay and HLS

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (typed seek primitives landed 2026-07-21; segment grid lowered into pharos-core 2026-07-25)
- **Deciders:** Alison

> Backfill: written retroactively from the code and the baseline spec, not at the time of the decision.

## Context

Each delivery path built its own seek response by hand: `deliver_stream` and
`serve_from_offset` for DirectPlay, `serve_segment` for HLS, `vp9_segment` for
VP9 fMP4. Nothing forced a path to answer a seek with a decodable,
self-consistent body, so every divergence was a separate bug. The module doc in
`seek.rs` lists the ones that triggered the change:

- A `Range: bytes=0-` answered `200`, which Firefox reads as "ranges unsupported" (B94, V47).
- A large `206` with its `Content-Length` stripped, forcing chunked framing.
- GET-open, GET-seek and HEAD computing `Content-Type` separately, so they could disagree.
- A StartTimeTicks resume cutting an mp4/mkv at an interior byte: a headerless, undecodable `206`.
- A segment index past the end of the media producing a `500` or a cached empty `200`.

ADR-0005 chose per-segment HLS and a fixed segment grid. This ADR covers how
every path, DirectPlay included, expresses a seek.

## Decision

Seek positions and byte ranges are expressed through shared types in one
module, `crates/pharos-server/src/api/jellyfin/seek.rs`, so the wrong
response cannot be built.

- **`ContentRange`** is the only way to build a partial body. Its status is hard-wired to `206`, its length is always known, and it is constructible only when the offset is inside the file. `ByteRange` parses the one `bytes=START-[END]` shape; suffix and multi-range requests fall through to `NamedFile`.
- **`DeliveryMime`** computes the source `Content-Type` once. GET-open, GET-seek and HEAD all call it. Only a genuine webm (by extension or a bare `webm` probe) is served as `video/webm`; a Matroska is never relabelled (B107, V57). A HEAD reports the real source length (B101, V48).
- **`CutTolerance` and `ResyncWitness`** gate a time-to-byte cut. Only self-framing containers (MPEG-TS, ADTS-AAC, MP3) yield a witness. For mp4/mkv/webm, and for any unknown container, the StartTimeTicks byte cut is skipped and the client seeks with its own index.
- **Out-of-bounds requests get a typed refusal, not a body.** A byte offset at or past EOF is `416`. A segment index past the end of the media is `404` ("segment index past end of media"), via `SegmentGrid::checked`. A segment index is constructible only in range.
- **One segment grid.** `SegmentGrid`, `SegmentIndex` and `segment_range` live in `pharos-core` and are re-exported through `seek::`. Segment boundaries carry a half-frame seek bias so the boundary frame belongs to exactly one segment.
- **`SeekableDelivery`** makes each path declare its cut tolerance and accuracy (`ByteExact` for DirectPlay, `SegmentGrid` at 6 s for HLS and VP9).

## Consequences

- The B94 class (`200` for a range, stripped length) and the headerless-slice class do not compile or cannot be reached from the shared types, rather than relying on review.
- A past-EOF byte range answers `416`, not `404`. The `404` applies to segment indices, and to an unreadable source file.
- The DirectPlay range cap (`DIRECTPLAY_RANGE_CAP_BYTES`, 8 MiB) is applied to browsers only. Native players size their read from `Content-Length`, so truncating for them ends playback at 8 MiB (B140, V94). The cap needs no `ResyncWitness`, because it returns a prefix of bytes the client asked for.
- Segment encode and playlist read the same window from `segment_range`, so they cannot describe different timelines. The half-frame bias fixed a frame dropped at every boundary (commit bb7fda3d, 2026-07-25); `bugs.md` has no entry for it.
- A segment bound is enforced only when the probe has a duration. A row without one keeps the old permissive behaviour (comment in `hls.rs`).
- A path that does not implement `SeekableDelivery` is not checked by the compiler. Rationale for the trait staying "deliberately small" is in the code comment only.

## References

- ADR-0005 (per-segment HLS; this ADR extends its segment grid to byte-range delivery)
- `crates/pharos-server/src/api/jellyfin/seek.rs`, `stream.rs` (`serve_from_offset`, `capped_window`, `DIRECTPLAY_RANGE_CAP_BYTES`), `hls.rs` (segment bounds check)
- `crates/pharos-core/src/segment_grid.rs` (`segment_seek_bias`, `segment_range`, `SegmentGrid`)
- `specs/001-pharos-baseline/invariants.md` V47, V48, V50, V57, V94
- `specs/001-pharos-baseline/bugs.md` B94, B101, B103, B107, B140
- Commits b5bbe163 (typed seek primitives), beafcf2a (one frame-snap grid), e0f56a62 (grid into pharos-core)
