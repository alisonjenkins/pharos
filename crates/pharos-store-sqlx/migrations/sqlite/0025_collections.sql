-- LIB-C5: collections / box sets as real entities.
--
-- Jellyfin's BoxSet groups a curated set of items (a film series, a
-- themed bundle) under one browsable folder. They arrive two ways:
--   1. NFO-driven — a movie's `<set>` / `<collection>` tag names the
--      box set it belongs to; the scanner persists the membership at
--      wire-in (idempotent, mirroring the genre/studio joins).
--   2. Manual CRUD — POST /Collections creates an (initially empty or
--      seeded) box set; POST/DELETE /Collections/{id}/Items add/remove
--      members. This is the Jellyfin client flow and has no NFO source.
--
-- collections.wire_id = the 32-hex collection_id_for(name) the Jellyfin
-- DTO emits as the BoxSet's `Id`, computed via pharos_core::
-- collection_wire_id at upsert (pure hash, never IO). Indexed so the
-- collection itself resolves by wire_id (`/Items/{wire_id}` → a BoxSet
-- BaseItemDto) and so `/Items?ParentId=<collection id>` pivots through
-- collection_items to the members. `kind` is the Jellyfin
-- CollectionType-ish discriminator (default 'boxset'); `overview` is the
-- optional synopsis a manual create may carry.
--
-- collection_items is the many-to-many membership join carrying a
-- per-link `sort_order` so the box set renders its members in a curated
-- order (NFO members append in scan order; manual adds append after the
-- current max). PK (collection_id, item_id) so adding the same item
-- twice is a no-op. No FK constraints (mirrors item_genres' additive-
-- only shape); a swept media_item leaves an orphan join row the read
-- path never resolves. Indexed on collection wire_id (the BoxSet +
-- ParentId pivot) + both join columns.
CREATE TABLE IF NOT EXISTS collections (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL UNIQUE,
    wire_id  TEXT NOT NULL,
    kind     TEXT NOT NULL DEFAULT 'boxset',
    overview TEXT
);

CREATE TABLE IF NOT EXISTS collection_items (
    collection_id INTEGER NOT NULL,
    item_id       INTEGER NOT NULL,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (collection_id, item_id)
);

CREATE INDEX IF NOT EXISTS idx_collection_items_collection_id ON collection_items(collection_id);
CREATE INDEX IF NOT EXISTS idx_collection_items_item_id ON collection_items(item_id);
CREATE INDEX IF NOT EXISTS idx_collections_wire_id ON collections(wire_id);
