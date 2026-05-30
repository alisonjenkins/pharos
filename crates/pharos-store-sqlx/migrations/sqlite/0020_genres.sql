-- LIB-C4: genres as real entities.
--
-- /Genres previously aggregated DISTINCT media_items.genre strings on
-- every request, and /Items?ParentId=<genre> resolved by re-hashing each
-- item's genre string in memory. We promote genres to rows so they carry
-- a stable wire id (and can hang future metadata off it), and so the
-- ParentId pivot is an indexed join instead of an in-memory DISTINCT scan.
--
-- genres.wire_id = the 32-hex genre_id_for(name) the Jellyfin DTO already
-- emits as a Genre's `Id`, computed at upsert time (the hash lives in
-- pharos-core, pure arithmetic, never IO). Indexed so /Items?ParentId=
-- <genre synth id> maps to the genre row by wire_id, then joins through
-- item_genres to the tagged items — exact, replacing the DISTINCT scan.
--
-- item_genres is the many-to-many join: one media_item carries several
-- genres (the source `genre` column is comma/pipe-separated). The probe
-- `genre` column on media_items is kept for back-compat (single-value
-- callers + the in-memory fallback path).
--
-- Backfill is done in code (the wire_id needs the hash), one-time on the
-- first /Genres request after upgrade (and the scanner populates
-- item_genres for newly-scanned items) — see GenreStore::backfill_genres.
CREATE TABLE IF NOT EXISTS genres (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    wire_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS item_genres (
    item_id  INTEGER NOT NULL,
    genre_id INTEGER NOT NULL,
    PRIMARY KEY (item_id, genre_id)
);

CREATE INDEX IF NOT EXISTS idx_item_genres_genre_id ON item_genres(genre_id);
CREATE INDEX IF NOT EXISTS idx_item_genres_item_id ON item_genres(item_id);
CREATE INDEX IF NOT EXISTS idx_genres_wire_id ON genres(wire_id);
