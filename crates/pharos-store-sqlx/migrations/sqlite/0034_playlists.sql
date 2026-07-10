-- T70: user-owned, ordered playlists. Unlike collections (a name-hashed
-- set), a playlist has a random wire_id, an owner, and may hold the same
-- item more than once — each membership is a distinct entry carrying its
-- own entry_id so the client's per-entry remove/reorder targets one slot.
CREATE TABLE IF NOT EXISTS playlists (
    id             INTEGER PRIMARY KEY,
    wire_id        TEXT NOT NULL UNIQUE,
    name           TEXT NOT NULL,
    owner_user_id  TEXT,
    media_type     TEXT NOT NULL DEFAULT 'Video',
    created_at     INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS playlist_items (
    playlist_id  INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    entry_id     TEXT NOT NULL,
    item_id      INTEGER NOT NULL,
    sort_order   INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, entry_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_playlist_items_order
    ON playlist_items (playlist_id, sort_order);
