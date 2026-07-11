-- Phase B4 (zero-downtime deploys): persist SyncPlay group state (see the
-- sqlite migration of the same name for rationale) so a live watch party
-- survives a rolling deploy — another replica re-hydrates the group from this
-- snapshot and takes over as coordinator when the previous owner drains.
CREATE TABLE IF NOT EXISTS sync_groups (
    group_id      TEXT PRIMARY KEY,
    epoch_unix_ms BIGINT NOT NULL,
    state_json    TEXT NOT NULL,
    updated_at    BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sync_groups_updated_at
    ON sync_groups (updated_at);
