-- T70: user-owned, ordered playlists (see the sqlite migration of the
-- same name for rationale).
CREATE TABLE IF NOT EXISTS playlists (
    id             BIGSERIAL PRIMARY KEY,
    wire_id        TEXT NOT NULL UNIQUE,
    name           TEXT NOT NULL,
    owner_user_id  TEXT,
    media_type     TEXT NOT NULL DEFAULT 'Video',
    created_at     BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS playlist_items (
    playlist_id  BIGINT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    entry_id     TEXT NOT NULL,
    item_id      BIGINT NOT NULL,
    sort_order   BIGINT NOT NULL,
    PRIMARY KEY (playlist_id, entry_id)
);

CREATE INDEX IF NOT EXISTS idx_playlist_items_order
    ON playlist_items (playlist_id, sort_order);
