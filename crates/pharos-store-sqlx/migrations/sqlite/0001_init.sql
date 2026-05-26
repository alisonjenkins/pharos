CREATE TABLE IF NOT EXISTS media_items (
    id    INTEGER PRIMARY KEY,
    path  TEXT    NOT NULL UNIQUE,
    title TEXT    NOT NULL,
    kind  TEXT    NOT NULL CHECK (kind IN ('movie', 'episode', 'audio'))
) STRICT;

CREATE INDEX IF NOT EXISTS idx_media_items_kind ON media_items(kind);
