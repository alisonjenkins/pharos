-- LIB-C5: collections / box sets as real entities (postgres mirror of
-- the sqlite 0025 migration). See the sqlite copy for the full rationale.
--
-- collections.wire_id = the 32-hex collection_id_for(name) the Jellyfin
-- DTO emits as the BoxSet's `Id`, computed via pharos_core::
-- collection_wire_id at upsert (pure hash, never IO). Indexed so the
-- BoxSet resolves by wire_id and `/Items?ParentId=<collection id>`
-- pivots through collection_items to the members in `sort_order`.
--
-- Postgres `INTEGER PRIMARY KEY` does NOT autoincrement (unlike sqlite's
-- rowid alias), so collections.id is a GENERATED IDENTITY column — the
-- upsert omits id and lets the backend assign it, then re-selects by the
-- UNIQUE name. collection_items.collection_id stays plain INTEGER (no FK
-- constraint, mirroring the sqlite shape + additive-only discipline);
-- item_id is BIGINT to match media_items' u64 id range. sort_order is
-- INTEGER carrying the curated member order.
CREATE TABLE IF NOT EXISTS collections (
    id       INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name     TEXT NOT NULL UNIQUE,
    wire_id  TEXT NOT NULL,
    kind     TEXT NOT NULL DEFAULT 'boxset',
    overview TEXT
);

CREATE TABLE IF NOT EXISTS collection_items (
    collection_id INTEGER NOT NULL,
    item_id       BIGINT NOT NULL,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (collection_id, item_id)
);

CREATE INDEX IF NOT EXISTS idx_collection_items_collection_id ON collection_items(collection_id);
CREATE INDEX IF NOT EXISTS idx_collection_items_item_id ON collection_items(item_id);
CREATE INDEX IF NOT EXISTS idx_collections_wire_id ON collections(wire_id);
