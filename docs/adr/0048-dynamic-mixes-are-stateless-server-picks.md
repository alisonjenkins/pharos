# ADR-0048: Dynamic mixes are picked by the server, statelessly, from client-held rules

- **Status:** Accepted
- **Date:** 2026-11-04T09:30:00Z
- **Deciders:** Alison (asked for the feature), Claude

## Context

Alison asked for an Amarok-style dynamic playlist: rules the next songs
should meet, with the queue topping itself up (spec 024). The rules read
play state (play counts, last played, favourites) that lives on the server,
and which songs a user may hear depends on library access and parental
rating, which only the server enforces. Pharos will have several
first-party clients (web now, phone and TV later) that should all offer the
same mixes.

## Decision

We will pick mix songs on the server, in a pure engine
(`services::mix::select`) behind one stateless native call,
`Library.mix(spec, recent, count)`. The client holds the mix spec and sends
what was heard or queued with each call; the server keeps no mix session.
Hard rules (*Only*, *Never*) never bend; freshness, shares and spacing bend
only when they would leave nothing, and every bend is named in the reply
and counted (`pharos_mix_relaxed_total{step}`). Each pick carries the rules
it matched as text. A user's saved mixes are one Prefs value (`mix.saved`)
holding a capnp `SavedMixes` message, not a new table.

## Consequences

- Every client gets the same picks and presets; the rules are one wire
  type (`MixSpec`), append-only like the rest of the schema.
- No server state to expire, replicate or clean up; a reload or another
  device carries on from the client's queue.
- Each call reads the user's whole visible music pool. Fine at home-library
  sizes, measured by `pharos_mix_seconds`; if it grows, hard rules get
  pushed into the store query without a wire change.
- Saved mixes have no server-side index or sharing between users; that
  needs a table and a new ADR if it is ever wanted.
- A new test kind (BPM, energy, similar artists) is a schema append plus an
  engine arm; old clients drop a test they don't know when reading saved
  mixes, and the server refuses a test it doesn't know rather than guess.

## Alternatives considered

- **Client-side picking** over a full library listing: ships every song's
  play state to the browser and duplicates the engine in every client.
- **A server-side mix session** (start/next/stop): state to expire and
  replicate across pods, for no gain over sending `recent`.
- **A constraint solver over the whole playlist** (Amarok's approach):
  harder to explain why a song was picked, and slow on big libraries; the
  per-pick share correction converges well enough (tested at ±8% over 200
  songs).
- **A `saved_mixes` table**: a migration and store methods on both
  backends for data only its owner reads; Prefs already stores per-user
  blobs.

## References

- `specs/024-music-mixes/spec.md`
- `crates/pharos-server/src/services/mix/`, `crates/pharos-client/src/mix.rs`
- B440, V380, V381
