# ADR-0108: The subtitle signal pass is stored per item

- **Status:** Proposed
- **Date:** 2026-11-10T08:40:00Z
- **Deciders:** Alison, Claude

## Context

The style director (spec 028, ADR-0067) reads per-line audio measures
(loudness, peak, pitch, voicing, high-band share) and scene cuts from a
libav signal pass that decodes the item's audio, about 60 s per episode at
background priority. Only the resulting effect plan was stored, so every
change to the director's rules (`DIRECTOR_VERSION`) had to decode the
whole styled library again, roughly 19 hours, before viewers saw the new
plans. Tuning the rules is expected to be frequent (B1204 is the first of
several).

## Decision

We will store each item's signal pass in its own table, `subtitle_signals`
(migration 0079), one row per item: the video file's signature, the audio
stream analysed, a `signals_version` naming the signal pass, and a JSON
body holding the line windows measured with their measures and cuts
(keyframes dropped). The styling job uses the stored pass when version,
signature, stream and windows all match, and otherwise measures and
replaces it. A pass with demux read errors is not stored. The store treats
the JSON as opaque; its shape belongs to `pharos-server`.

## Consequences

- A director change re-directs the library in minutes of CPU instead of a
  day of decoding, once each item has been measured under this scheme.
- The first director bump after this lands still measures everything, to
  fill the table.
- Changing the signal pass's maths needs a `SIGNALS_VERSION` bump, or the
  director will keep reading old measures.
- Storage: a few KB to tens of KB of JSON per styled item.

## Alternatives considered

- A column on `subtitle_styling`: that row is keyed by the source track and
  replaced when the source choice changes, while the signals depend on the
  video and the line windows; and its plan reads would carry the extra
  bytes.
- A binary encoding: smaller, but JSON is readable when debugging a
  director decision and the size is modest.

## References

- spec 028, B1202, V1162
- `crates/pharos-server/src/subtitle_styling.rs`
