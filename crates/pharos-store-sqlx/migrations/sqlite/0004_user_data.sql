-- T33: per-(user, item) playback state. Drives the watched indicator,
-- play_count, the Resume tile, and the favourite star in jellyfin-web.
-- Composite primary key — one row per pair; missing row == defaults.
CREATE TABLE IF NOT EXISTS user_data (
    user_id                    BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id                    INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    played                     INTEGER NOT NULL DEFAULT 0 CHECK (played IN (0, 1)),
    play_count                 INTEGER NOT NULL DEFAULT 0,
    -- Jellyfin's 100ns ticks; 10_000_000 per second.
    last_played_position_ticks INTEGER NOT NULL DEFAULT 0,
    is_favorite                INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
    last_played_at             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, item_id)
) STRICT;

-- Index on user_id alone for /Users/{u}/Items/Resume (filter by user,
-- scan for non-zero position).
CREATE INDEX IF NOT EXISTS idx_user_data_user ON user_data(user_id);
