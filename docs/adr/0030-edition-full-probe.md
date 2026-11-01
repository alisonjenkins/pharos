# ADR-0030: Each grouped edition keeps its full probe in `alternate_sources_json`

- **Status:** Accepted
- **Date:** 2026-11-01T21:42:39Z
- **Deciders:** Alison

## Context

P34 groups an item's other editions (a 3D rip, a director's cut) under the
primary file, and PlaybackInfo advertises each as a `MediaSourceInfo`.
Clients pick one and echo its id back as `MediaSourceId`, but no byte route
could serve an edition: the stored `AlternateMediaSource` held only summary
fields (container, codecs, bitrate, size, duration, name, 3D tag). Playing a
file needs its streams, tracks and dimensions, and the 3D decisions from
spec 018 need the edition's own stream signals, not the primary's.

The scanner already runs a full probe on every edition before collapsing it,
so the data exists at scan time and is thrown away.

## Decision

We will store each edition's full `MediaProbe` as
`AlternateMediaSource::probe: Option<Box<MediaProbe>>`, inside the existing
`alternate_sources_json` column, with no new column or migration. The
field is omitted from JSON when `None` and defaults to `None` on decode.
The edition's 3D layout is resolved from that probe, falling back to the
filename tag. `PROBE_SCHEMA_VERSION` goes to 9 so the next scan re-probes
rows written without it.

## Consequences

- A playback route can swap to an edition's path and probe without probing
  on the request path.
- Rows from before v9 decode with `probe: None` until re-probed. A request
  for such an edition is served the primary file and recorded as such:
  pairing the primary's stream list with another file would hand ffmpeg
  stream indices that do not exist in it.
- `alternate_sources_json` grows by one probe per edition (its stream list
  is the bulk). Items with editions are a small fraction of a library.
- The summary fields now duplicate parts of `probe`. They stay, because
  PlaybackInfo and pre-v9 rows read them.

## Alternatives considered

- **Probe the edition on demand at playback.** Puts an ffprobe on the
  request path of every edition play and re-derives what the scanner
  already knew.
- **Store each edition as its own hidden `media_items` row.** Gives every
  edition a full row, but every listing, count and watch-state query would
  have to exclude the hidden rows, and user data would need mapping
  between ids.

## References

- `crates/pharos-core/src/lib.rs` (`AlternateMediaSource`, `PROBE_SCHEMA_VERSION`)
- `crates/pharos-scanner/src/fs.rs` (`probe_with_alternates`)
- `specs/018-3d-media/`
