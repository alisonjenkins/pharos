-- B1490: which native grant owns a play session, so a renewal after a
-- rollout can take the session over only when no grant on any replica still
-- holds it (ADR-0150).
--
-- grant_epoch counts the grants that have owned the session: each claim adds
-- one, in the same statement that checks the session is free, and a stop, a
-- holder change or a lease renewal from a grant names the epoch it holds, so
-- one from a grant the session has since moved on from changes nothing.
-- 0 = never claimed: a session negotiated before this column, or by a path
-- that takes no lease, which no renewal may take over.
--
-- grant_lease_until (unix seconds) is how long the owning grant's replica has
-- the session without renewing; NULL once it let the session go. A renewal
-- takes the session over only after the lease has lapsed or been let go.
--
-- grant_source is the resolved path of the file the session plays (the
-- primary file's path, or an edition's), written by the first claim and
-- matched by a takeover: two editions can share one probe, so the probe
-- alone does not say it is the same file. NULL = not known (never claimed).
ALTER TABLE transcode_sessions ADD COLUMN grant_epoch BIGINT NOT NULL DEFAULT 0;
ALTER TABLE transcode_sessions ADD COLUMN grant_lease_until BIGINT;
ALTER TABLE transcode_sessions ADD COLUMN grant_source TEXT;
--
-- transcode_session_ends remembers, for a while after a claimed session's
-- row is removed, the grant epoch it was at, written with the removal: a
-- grant's stop, the session cap's eviction or trim, or any other end. A
-- grant whose stop then finds the row gone tells from it whether the session
-- ended while it was still that grant's (its own stop whose answer was lost,
-- or the cap) or after a renewal took it over. Pruned with the sessions.
CREATE TABLE transcode_session_ends (
    play_session_id TEXT PRIMARY KEY NOT NULL,
    grant_epoch BIGINT NOT NULL,
    ended_at BIGINT NOT NULL
);
