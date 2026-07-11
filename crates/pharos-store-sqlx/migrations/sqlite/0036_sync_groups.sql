-- Phase B4 (zero-downtime deploys): persist SyncPlay group state so a group
-- survives a rolling deploy. When the replica owning a group's actor drains,
-- another replica re-hydrates the group from this snapshot and takes over as
-- the new coordinator, so a live watch party keeps playing across the cutover.
--
-- `state_json` is an opaque serde payload owned by the pharos-sync layer (the
-- whole group coordination state: leader, playback anchor, queue, member
-- roster, group name, readiness gate) — the store treats it as text, keeping
-- the schema codec/protocol-agnostic. `epoch_unix_ms` is the group's wall-clock
-- time base, stored separately so a new owner derives the SAME monotonic
-- `server_ms` clock and already-scheduled instants stay absolute across the
-- handoff. `updated_at` is unix-seconds, driving the idle-group pruner.
--
-- SQLite is single-replica (single-writer), so in practice its groups never
-- leave the process; this table exists for backend parity + testability.
CREATE TABLE IF NOT EXISTS sync_groups (
    group_id      TEXT PRIMARY KEY,
    epoch_unix_ms INTEGER NOT NULL,
    state_json    TEXT NOT NULL,
    updated_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_sync_groups_updated_at
    ON sync_groups (updated_at);
