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
  - no grant on this replica holds that id, no running lease on the
    session's row (in the shared store, migration 0082) covers it, and this
    replica's /Sessions lists no entry with it. Each grant holds its session
    from its negotiate to its stop or the end of its release grace, in
    memory on its replica, and its replica keeps the row's lease running.
    A grant on another replica can therefore still be live once its lease
    has lapsed or a drain let it go (see Consequences);
  - a stored transcode session has that id;
  - it is held by the same user, for the same item;
  - its source probe, decision and burned subtitle indices equal the new
    negotiation's, and its lease was taken playing the same file (the
    resolved path, since two editions can share a probe);
  - the store's takeover wins (below).
- **The lease.** Each session row gains `grant_epoch` (0 = never leased),
  `grant_lease_until` and `grant_source`. The first grant on a new session
  leases it (epoch 0 to 1) once the session is stored. Its replica renews
  every lease its grants hold every 10 s for 30 s. The last grant on that
  replica to let the session go releases the lease (sets it to NULL), and a
  draining replica releases all of them at SIGTERM. A crashed replica, or
  one that lost the store, stops renewing and its leases lapse after 30 s.
  No database connection is held for any of it.
- **The takeover.** A renewal takes the session over in one `UPDATE`: only
  while the session was leased before, plays the same file, is the same
  user's, is still live, and its lease has lapsed or been released. The
  same statement makes the renewing token its holder, moves its epoch on
  and starts the new grant's lease. Two renewals on two replicas therefore
  cannot both win. A session no grant ever leased (one negotiated before
  this change, or whose lease failed) is never taken over. A first lease
  that errors may still have committed (its reply lost), so the grant
  keeps epoch 1 either way: if the lease committed, the grant renews and
  fences like any other; if not, its first renewal finds nothing and stops.
- **Fencing.** A grant's stop deletes the row only while the session's
  epoch has not moved past the grant's, in the `DELETE` itself; so do the
  release of a grant nobody adopted and a holder change on adoption, while
  a lease renewal and a lease release need the exact epoch. "Not moved
  past" lets a grant fenced at 1 whose lease never committed (the row
  still at 0) end its own stream, and still shuts it out once a takeover
  moves the epoch to 2. A grant the session has moved on from is fenced out of all
  of them, while the current grant, whichever token it used, still stops
  the stream. A fenced stop stores no position and leaves the /Sessions
  entry to the grant that plays the stream now. The grant keeps the epoch
  it was negotiated at apart from its hold, so a repeated stop is fenced
  too. Its progress reports read the session's epoch first and skip the
  write when that read shows the session has moved on
  (`pharos_play_grant_superseded_total{what}`). A row gone with a removal
  remembered at this grant's epoch (`transcode_session_ends`) still counts
  as this grant's, and the report goes ahead. When the row is gone with no
  such removal, or the read fails, the store cannot say, and the report
  counts as superseded too: this replica's view of its lease is a snapshot by its
  own clock, which a takeover elsewhere (going by its clock) does not
  respect, so it cannot stand in for the store's word. The next report
  the store answers for stores the position. The epoch read is best effort,
  not a fence: a takeover can land after the read, and the report then
  still stores the same user's resume point for the same stream. The new
  grant's next report or its stop overwrites it. Held progress is
  coalesced per process, so two replicas' reports are not merged, but a
  deferred write only lands if it is newer than the stored one. Only the
  writes that end or take over a stream carry the epoch in the statement.
  A stop goes on to store the position only with evidence the session was
  its own: its end removed the row now, or an earlier stop of the same
  grant did (a retry after the position write failed), or the store
  remembers the row was removed at this grant's epoch. Every removal of a
  claimed row (a grant's stop, the session cap's eviction or trim, a REST
  stop, the retention prune) records an epoch in `transcode_session_ends`,
  in the removal's own transaction: the row's own, except that a grant's
  fenced stop records the grant's epoch (a grant whose first lease errored
  stops a row still at 0). The prune forgets old records. So a stop
  retried after its delete reply was lost, and a stop after the cap or the
  prune ended the session, still store, and reports read the same record.
  The stop reads its row with no retention cutoff, since lease renewals do
  not touch `updated_at`. A row gone otherwise (ended by a grant that took
  it over since; a local lease snapshot cannot tell), another user's, or a read
  of it that failed stores nothing; the failed read answers unavailable, so the
  release retries it. Only a stop that ended the playback lets the hold
  go: one that found the row gone without that evidence (an overlapping
  stop of the same grant may have removed it and may yet fail to store)
  leaves the release to retry with its position. A failed stop's own
  position is what that release saves, kept apart from the last report so
  a report already on its way cannot put it back behind the stop. A drain also waits for the
  releases the last holds began just before it. A takeover that commits
  but that the renewal does not keep (it landed lapsed and could not be
  renewed, another grant won it, or a drain began before it was attached)
  leaves the row in place, so the stream the player is still fetching keeps
  playing until it renegotiates, as any renewal that mints its own session
  leaves it. Its lease is let go (by the drain, or when the hold drops), so
  a replacing replica's renewal can take the session over, and the grant
  before it stays fenced out by the epoch the takeover moved on.
- A kept name thus rebinds the stored session's holder to the new grant.
  The server opens no new transcode session and prewarms nothing.
- Any other name is ignored. The renewal gets a new session, and refusing
  the name ends nothing, which is today's behaviour. Registering that new
  session can still evict an older one when the user is at the session cap,
  ending its listing and prefetch, as any negotiate can (V1481).
- Each named renewal records `pharos_play_session_resume_total{outcome}`
  (resumed, live, gone, not_held, other_stream, unleased, failed) and logs
  the outcome with its reason. A refused takeover reads the lease back to
  say why: its epoch, how long it still runs, or that it plays another
  file. Leases record `pharos_play_session_claim_total{outcome}` (claimed,
  refused, failed, draining) and `pharos_play_session_lease_total{outcome}`
  (renewed, lost, renew_failed, released, release_moot, release_failed).
  A fenced stop counts as `pharos_play_stop_total{outcome="skipped",reason="superseded"}`.

The stored session holds no audio track: the audio rides on the stream URL
(`AudioStreamIndex`). The decision equality covers the audio codec target,
and the player's own reports carry the tracks it plays, so the audio track
is not compared.

## Consequences

- A rollout leaves one /Sessions entry per stream. Stop still ends the
  stream, as the kept session is the one the stream uses.
- No two grants hold overlapping running leases on one session. A renewal
  that loses the takeover mints a new session.
- Two live grants can name one session in two ways. A grant whose lease
  lapsed while it still lived (its replica lost the store for 30 s) may see
  a renewal elsewhere take the session over. And a draining replica lets
  its leases go while its grants still live, so the replica replacing it
  can take a session over before the old grant drops. Either way, from the
  takeover on the old grant owns nothing: the session answers to the new
  grant's token, and the old grant's stop, the end of its release grace,
  its holder change and its lease writes are all fenced out by the epoch
  in the statement. After a lapse, its replica's next renewal that reaches
  the store matches nothing, so it stops renewing it
  (`lease_total{outcome="lost"}`); a renewal the store fails to answer is
  counted `renew_failed` and tried again next round, harmlessly, as the
  epoch fences it out too. A draining replica has stopped renewing
  already. That fence is the hard guarantee; the
  old grant's progress reports are only checked first, best effort, as
  described above.
- A lease write that waited for a connection or a row lock can land with
  its deadline, taken before it, already past. The grant then renews it at
  once, and a takeover that lost to another in that moment keeps nothing;
  one whose renewal then fails is not kept either (`resume` outcome
  `failed`), and its hold lets the lapsed lease go. A renewal counts as
  holding the lease only when it committed before its own deadline. Each
  renewal is bounded by a 5 s timeout and they run side by side, so one
  stalled on the store cannot hold the others past their deadlines.
- Leases cost one short `UPDATE` per held session every 10 s, on the query
  pool, and no connection is kept. Replicas' clocks must agree to well
  within the 30 s lease; a pod's clock is NTP-synced.
- A draining replica lets every lease go at SIGTERM and takes none after.
  A renewal that reaches the replica replacing it during the drain can then
  keep its stream's session, and one that still reaches the draining
  replica mints a new one there. A lease taken while the drain began is let
  go again and the renewal refused, and a takeover is kept only if, at the
  moment it is accepted and under the holds' lock, its lease still runs on
  this replica and no drain has begun.
- During the rollout that brings this change in, sessions the old replicas
  negotiated carry no lease, so no renewal keeps them. Renewals behave as
  before for them, and the first stop or restart after clears them.
- The release of the last hold happens on a spawned task, so for a moment
  after a grant drops, a renewal can still find its lease running and mint
  a new session: the safe direction.
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
- **Store the audio track on the transcode session.** The decision and
  the player's reports already cover it.
- **A Postgres advisory lock per grant, on a connection the replica
  keeps.** This was the first version. Each live grant kept a database
  connection outside the pool's budget, a lost connection silently let the
  lock go while the grant played on, the lock could not fence a stale stop
  or holder change, and SQLite had no cross-replica exclusion at all. A
  lease and epoch on the row fix all four.

## References

- B452, B1090, B1482, B1490; V392, V1473, V1481.
- `crates/pharos-server/src/services/playback.rs` (`resume_named`),
  `crates/pharos-server/src/services/play_holds.rs` (`PlayHolds`),
  `crates/pharos-store-sqlx/migrations/*/0082_play_session_lease.sql`.
