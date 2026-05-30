-- LIB-C4: genres as real entities (postgres mirror of the sqlite 0020
-- migration). See the sqlite copy for the full rationale.
--
-- genres.wire_id = the 32-hex genre_id_for(name) the Jellyfin DTO emits,
-- computed at upsert time (pure hash in pharos-core, never IO). Indexed
-- so /Items?ParentId=<genre synth id> resolves by an indexed join through
-- item_genres rather than an in-memory DISTINCT scan. item_genres is the
-- many-to-many join; the probe `genre` column stays for back-compat.
--
-- Postgres `INTEGER PRIMARY KEY` does NOT autoincrement (unlike sqlite's
-- rowid alias), so genres.id is a GENERATED IDENTITY column — the upsert
-- omits id and lets the backend assign it, then re-selects by the UNIQUE
-- name. item_genres.genre_id stays plain INTEGER (no FK constraint,
-- mirroring the sqlite shape + additive-only discipline); item_id is
-- BIGINT to match media_items' u64 id range.
CREATE TABLE IF NOT EXISTS genres (
    id      INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    wire_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS item_genres (
    item_id  BIGINT NOT NULL,
    genre_id INTEGER NOT NULL,
    PRIMARY KEY (item_id, genre_id)
);

CREATE INDEX IF NOT EXISTS idx_item_genres_genre_id ON item_genres(genre_id);
CREATE INDEX IF NOT EXISTS idx_item_genres_item_id ON item_genres(item_id);
CREATE INDEX IF NOT EXISTS idx_genres_wire_id ON genres(wire_id);
