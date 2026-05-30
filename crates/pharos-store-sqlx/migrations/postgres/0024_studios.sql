-- LIB-C3: studios as real entities (postgres mirror of the sqlite 0024
-- migration). See the sqlite copy for the full rationale.
--
-- studios.wire_id = the 32-hex studio_id_for(name) the Jellyfin DTO emits,
-- computed at upsert time (pure hash in pharos-core, never IO). Indexed
-- so /Items?ParentId=<studio synth id> resolves by an indexed join
-- through item_studios rather than aggregating album_artist in memory.
-- item_studios is the many-to-many join; the probe carries no legacy
-- studio column, so there is no backfill — the scanner populates the join
-- from MetadataResult on write.
--
-- Postgres `INTEGER PRIMARY KEY` does NOT autoincrement (unlike sqlite's
-- rowid alias), so studios.id is a GENERATED IDENTITY column — the upsert
-- omits id and lets the backend assign it, then re-selects by the UNIQUE
-- name. item_studios.studio_id stays plain INTEGER (no FK constraint,
-- mirroring item_genres + additive-only discipline); item_id is BIGINT to
-- match media_items' u64 id range.
CREATE TABLE IF NOT EXISTS studios (
    id      INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    wire_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS item_studios (
    item_id   BIGINT NOT NULL,
    studio_id INTEGER NOT NULL,
    PRIMARY KEY (item_id, studio_id)
);

CREATE INDEX IF NOT EXISTS idx_item_studios_studio_id ON item_studios(studio_id);
CREATE INDEX IF NOT EXISTS idx_item_studios_item_id ON item_studios(item_id);
CREATE INDEX IF NOT EXISTS idx_studios_wire_id ON studios(wire_id);
