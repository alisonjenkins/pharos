# ADR-0023: Remote media sources resolved on demand

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (designed 2026-08-01; backfilled 2026-10-05)
- **Deciders:** Alison

> Backfill: written retroactively from `specs/008-remote-sources/spec.md` and
> the code, to record a decision already embodied in the codebase. The spec
> does not record a live verification against a real site.

## Context

The goal of spec 008 was to sit down with friends and watch anything together,
YouTube included. The spec found most of it already solved. The SyncPlay group
actor treats queue entries as opaque id strings and never resolves one against
the database (ADR-0014), and stock jellyfin-web plays a non-direct-play item
through hls.js like any library file. The missing piece was a media item with
no file behind it, and only the server needed to change.

The HLS path already forwards `item.path` to ffmpeg as an opaque string, and
libav opens an `https://` input itself. The open questions were what the item
is, when its URL is fetched, and what happens to cached bytes when the URL
changes.

## Decision

**A remote item is a normal library row whose path is a stable synthetic
`ytdlp://<extractor>/<id>`. Its real media URLs are resolved with `yt-dlp` at
play time, memoised for a TTL, and never stored.**

- **Representation.** `MediaItem.path` stays a path. `Origin` (Local or
  Remote) is a typed accessor and `LocalPath` is unforgeable, so a remote item
  cannot reach a filesystem call without the compiler objecting
  (`pharos-core/src/origin.rs`). The reason a stored signed URL is unusable is
  that it rotates and `media_items.path` is `UNIQUE`. `POST /Pharos/Remote/Items`
  is idempotent: the same URL yields the same id.
- **Resolution, not download.** ffmpeg is handed yt-dlp's range-capable
  format URLs, so a seek is an HTTP range request. A download-then-play design
  was rejected: yt-dlp merges video and audio parts only at the end, so there
  is never a growing file to play from. Fetches go through a byte-range
  read-through cache served over loopback, so each segment encode does not
  reopen an HTTPS connection (`remote/source_cache.rs`).
- **When URLs are resolved and evicted.** `ResolverCache::locate` reuses a
  resolution for `[remote].resolve_ttl_secs` (default 1800). After the TTL it
  re-resolves, and only if the locator changed does it release the bytes cached
  under the old URL. A re-resolve returning the same URL keeps its warm cache.
  A locator found dead before its TTL is dropped through `invalidate`. Deleting
  a library calls `forget_all`. The cache is also capped (`cache_max_bytes`,
  default 8 GiB) and emptied at startup, because a sparse file's holes read as
  zeros and a bitmap cannot be trusted across a restart.
- **Direct play is refused by design.** A remote item advertises
  `supports_direct_play: false` with the new `DirectPlayBlock::RemoteSource`
  reason, so it never lands in the "media is missing" bucket of
  `pharos_source_unreadable_total`.
- **Cache identity.** A per-item `SourceGen` derived from the locator goes into
  the segment disk key, the ETag and the query string of every `immutable` URI,
  so a re-resolved source cannot be served from a year-old browser entry
  (V132). It cannot ride the process-wide `pharos_cache::generation()`, which
  would wipe every local title.

Off by default: `[remote].enabled = false`, because restreaming from most sites
is contrary to their terms of service.

## Consequences

- Group watch and stock clients need no changes; any URL pasted becomes a
  queueable item id.
- Playback depends on `yt-dlp` and the upstream site. A locator that dies
  mid-play costs a re-resolve, not a failed item.
- Four rules came with it. Background sweeps must decline an item they cannot
  process instead of retrying forever (V134). A network fetch meters on its own
  `NetworkGate`, not the disk-oriented `bg_io` gate (V135, ADR-0017). Remote
  items sit under the library root `ytdlp:`, which no scan walks (V136); parked
  under a real root, a scan would delete them all below B98's blast-radius
  guard. The mpegts rung also had to carry `s=` (B181).
- Ingestion differs from the spec text. The spec says to stamp `library_id`
  directly; the code calls `backfill_library_ids` after the `put`, to keep one
  assignment mechanism.
- The built-in Dioxus UI cannot play a transcoded item or SyncPlay (no
  hls.js, no websocket in `crates/pharos-ui`). That is out of scope here and is
  assigned to a future spec 009.
- Subtitle tracks are not populated for remote items (spec: the resolver
  populates none).

## References

- `specs/008-remote-sources/spec.md`; invariants V132, V134, V135, V136
  (V133 withdrawn in the spec); bug B181 in `specs/001-pharos-baseline/`
- ADR-0014 (SyncPlay), ADR-0017 (background-I/O gate)
- `crates/pharos-server/src/api/pharos/remote_items.rs`,
  `src/remote/mod.rs` (`ResolverCache`), `src/remote/source_cache.rs`,
  `src/bg_io.rs` (`NetworkGate`), `src/config.rs` (`RemoteConfig`),
  `crates/pharos-server/tests/remote_end_to_end.rs`
