-- 023: where a user left off in a book. Apart from user_data so a reader's
-- locator can never overwrite last_played_position_ticks (V354).
CREATE TABLE IF NOT EXISTS read_positions (
    user_id    BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id    INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    -- Opaque to the server; the reader's own format, 256 bytes at most.
    locator    TEXT    NOT NULL,
    -- Whole-book progress, 0 to 1.
    fraction   REAL    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_id)
) STRICT;

-- "Continue reading": one user's saves, newest first.
CREATE INDEX IF NOT EXISTS idx_read_positions_recent ON read_positions(user_id, updated_at DESC);
