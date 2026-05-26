-- T33: per-(user, item) playback state. Drives the watched indicator,
-- play_count, the Resume tile, and the favourite star in jellyfin-web.
CREATE TABLE IF NOT EXISTS user_data (
    user_id                    BYTEA   NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id                    BIGINT  NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    played                     INTEGER NOT NULL DEFAULT 0 CHECK (played IN (0, 1)),
    play_count                 INTEGER NOT NULL DEFAULT 0,
    last_played_position_ticks BIGINT  NOT NULL DEFAULT 0,
    is_favorite                INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
    last_played_at             BIGINT  NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, item_id)
);

CREATE INDEX IF NOT EXISTS idx_user_data_user ON user_data(user_id);
