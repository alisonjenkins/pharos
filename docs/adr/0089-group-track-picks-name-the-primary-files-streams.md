# ADR-0089: A group's track picks name the primary file's streams

- **Status:** Accepted
- **Date:** 2026-11-09T18:40:00Z
- **Deciders:** Alison Jenkins

## Context

A Together group carries one audio and one subtitle pick
(`GroupTracksView`), each a stream index, and every member following the
group's tracks applies it. Since ADR-0079 and ADR-0080 one item can play
from several files, and spec 034 lets each member play the version their
device picked: one on the 2D copy, one on the 3D. The two files' streams are
laid out independently, so one index names different streams in each, and a
pick from the 2D file moved a 3D member to another language (B941).

## Decision

An index in the group's track picks names a stream of the item's primary
file. A member playing another version maps its own stream to the primary's
before it sends a pick, and the primary's to its own before it applies one.
Two streams match when they share language, forced flag and
hearing-impaired flag: first those whose title, codec and channel count
agree as well, then the rest in order among the unpaired streams that
share them. A stream with no match is not sent or applied,
and the client logs `pharos: group tracks unmapped`. The mapping lives in
the client (`track_follow`); the server stores and relays indexes unchanged.

## Consequences

- Members on different versions keep one language, which is what following
  the group's tracks means.
- No wire change and no schema bump: a member on the primary sends and
  applies what it did before. A client from before this change on another
  version still sends its own index, as it did.
- A same-language commentary pairs with its twin by title and codec, so it
  cannot take the main track's place. Two same-language streams whose
  titles and codecs differ in both files still pair by order, and can
  cross; the member can take their own tracks.
- Every member fetches the primary's tracks, which it already does to learn
  the versions.

## Alternatives considered

- Carry language and flags in the group's pick instead of an index: a
  schema and server change for every client, and the group's late-joiner
  start (`tracks`) would need the same.
- Each member follows the group only when on the picker's version: mixed
  groups would never share tracks.

## References

- `crates/pharos-ui/src/native/track_follow.rs`
- Spec 034 US2; B941, V902 in `specs/001-pharos-baseline/`
