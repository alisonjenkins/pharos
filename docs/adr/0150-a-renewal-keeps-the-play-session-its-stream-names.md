# ADR-0150: A renewal keeps the play session its stream names

- **Status:** Accepted
- **Date:** 2026-11-20T20:10:00Z
- **Deciders:** Alison Jenkins

## Context

A native client renews its playback grant after its socket reconnects
(B452). The stream keeps going on the URLs it already has, which carry the
old play session id as a capability (B1090).

The server only keeps that id when it can adopt a released grant. Releases
live in memory for a 30 s grace (`pending_release`). At shutdown, grants
are left as they are, so after a rollout no release exists. The stored
transcode session does outlive the restart (it is in the DB, 6 h
retention), so the old stream still plays.

The renewal therefore gets a new play session. /Sessions lists it and the
player's reports, while the stream runs on the old one. One viewer shows as
two sessions, and the client marks its working stream suspect (B1482).

## Decision

We will let a renewal name the play session its stream uses, and keep it
when the server can prove it is that stream's own.

- `Playback.negotiate` gains `resumes :PlaySessionId` (schema version 49).
  The id is a `$secret` and is redacted like every other play session id.
- The server keeps the name only when all of these hold:
  - no live /Sessions entry has that id, so two grants never share one;
  - a stored transcode session has that id;
  - it is held by the same user, for the same item;
  - its source probe (which pins the version), decision and burned
    subtitle indices equal the new negotiation's.
- A kept name rebinds the stored session's holder to the new grant. The
  server opens no new transcode session and prewarms nothing.
- Any other name is ignored. The renewal gets a new session and nothing is
  ended, which is today's behaviour.
- Each named renewal records `pharos_play_session_resume_total{outcome}`
  (resumed, live, gone, not_held, other_stream, failed) and logs the
  outcome with its reason.

The stored session holds no audio track: the audio rides on the stream URL
(`AudioStreamIndex`). The decision equality covers the audio codec target,
and the player's own reports carry the tracks it plays, so the audio track
is not compared.

## Consequences

- A rollout leaves one /Sessions entry per stream. Stop still ends the
  stream, as the kept session is the one the stream uses.
- The client stops marking a rollout's renewal suspect, so B1482's
  renegotiation is saved for a session that really ended.
- An older server ignores the field (capnp default), so a new client still
  works against it. An older client sends no name and behaves as before.
- Knowing a play session id is now also enough to name it in a renewal.
  That grants nothing new: the id is already the stream capability, and the
  holder must be the same user.

## Alternatives considered

- **End the old session when the renewal does not adopt.** This stops the
  playing stream and remounts the player, breaking V392.
- **Persist the release table.** It would cover only grants released
  before shutdown, and shutdown does not release them.
- **Store the audio track on the transcode session.** This needs a schema
  migration for a check the decision and the player's reports already
  cover.

## References

- B452, B1090, B1482, B1490; V392, V1473, V1481.
- `crates/pharos-server/src/services/playback.rs` (`resume_named`).
