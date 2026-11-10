# ADR-0106: A daily sweep repairs artwork rows whose file is gone

- **Status:** Proposed
- **Date:** 2026-11-10T07:30:00Z
- **Deciders:** Alison

## Context

An `artwork` row records a file path for an item's role. Nothing revisits the
row after it is written: the scanner re-resolves artwork only when it
re-probes a media file, and `persist_artwork` only upserts. A row therefore
outlives its file when the sidecar is deleted, or when the row predates B571
and names the media server's container path. The image route then serves a
video frame, `has_primary_art` stays true, and online enrichment treats the
dead row as curated art, so the item never gets a real poster (B1180, B1181).

## Decision

We will run a `stale_artwork` sweep on the background leader, five minutes
after boot and then daily. It pages through every item-servable artwork row
and, for each file that is definitely not found:

- relinks a `local` row to a same-named file in the item's own folder (for an
  episode, also the folder above), never higher;
- otherwise deletes the row, clears `has_primary_art` for a Primary, bumps
  `art_version`, and re-admits the item for online matching unless its match
  is `manual`;
- leaves the row alone when the item's own media file cannot be seen either,
  since that means the share is down rather than the art deleted;
- removes rows whose item no longer exists.

`[metadata].repair_stale_artwork` (default `true`) turns repair off, leaving
detection and its metric. Separately, `enrich_one` counts a local row as
curated only while its file exists, under the same outage guard.

## Consequences

- Dead rows heal without a rescan, and items whose sidecar was deleted get
  provider art on the next metadata pass.
- The sweep stats every servable artwork row once a day over the media share.
- The first pass after deploy can re-admit many items at once; the metadata
  backfill's existing pacing spreads the provider downloads.
- A deleted local sidecar is replaced by provider art. A user who deletes a
  poster on purpose to get a frame instead cannot do that any more.

## Alternatives considered

- Re-resolve artwork on every scan for unchanged files: far more I/O per scan,
  and it still would not re-admit items for online art.
- Fix rows in the image route when it finds them missing: writes from a public
  GET route, and only for items someone happens to view.
- A one-off migration: misses rows that go dead later.

## References

- `crates/pharos-server/src/stale_artwork.rs`
- B571, B1180, B1181; V484, V1140, V1141
