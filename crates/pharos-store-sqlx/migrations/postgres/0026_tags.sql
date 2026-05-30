-- LIB-C6: tags as real entities (postgres mirror of the sqlite 0026
-- migration). See the sqlite copy for the full rationale.
--
-- tags.wire_id = the 32-hex tag_id_for(name) the Jellyfin DTO emits,
-- computed via pharos_core::tag_wire_id at upsert (pure hash, never IO).
-- Indexed so `/Items?ParentId=<tag synth id>` resolves by an indexed join
-- through item_tags rather than an in-memory scan. item_tags is the
-- many-to-many join; tags are populated by the scanner wire-in + the
-- manual add/remove endpoints (no legacy column → no backfill).
--
-- Postgres `INTEGER PRIMARY KEY` does NOT autoincrement (unlike sqlite's
-- rowid alias), so tags.id is a GENERATED IDENTITY column — the upsert
-- omits id and lets the backend assign it, then re-selects by the UNIQUE
-- name. item_tags.tag_id stays plain INTEGER (no FK constraint, mirroring
-- the sqlite shape + additive-only discipline); item_id is BIGINT to
-- match media_items' u64 id range.
CREATE TABLE IF NOT EXISTS tags (
    id      INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    wire_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS item_tags (
    item_id BIGINT NOT NULL,
    tag_id  INTEGER NOT NULL,
    PRIMARY KEY (item_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_item_tags_tag_id ON item_tags(tag_id);
CREATE INDEX IF NOT EXISTS idx_item_tags_item_id ON item_tags(item_id);
CREATE INDEX IF NOT EXISTS idx_tags_wire_id ON tags(wire_id);
