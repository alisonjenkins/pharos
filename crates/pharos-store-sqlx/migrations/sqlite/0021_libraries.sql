-- LIB-C1: typed libraries as real entities.
--
-- The /Library/VirtualFolders + /Library/MediaFolders endpoints previously
-- returned a single hardcoded "All Media / mixed" stub (or synthesised one
-- "mixed" CollectionFolder per [media].roots entry). We promote libraries
-- to rows so each carries a typed `kind` (movies/tvshows/music/mixed) that
-- drives the Jellyfin CollectionType, a stable `wire_id`, and so the
-- /Items?ParentId=<library id> pivot becomes an indexed library_id lookup
-- instead of an in-memory path-prefix scan over every item.
--
-- libraries.wire_id = the existing 32-hex library_id_for_root(root) the
-- views/virtual-folder DTOs already emit as a library `Id`, computed at the
-- API boundary (pharos_scanner::stable_id over the canonical path) and
-- stamped here at reconcile time so existing client URLs survive.
--
-- media_items.library_id (nullable, additive) references libraries.id; it
-- is backfilled by a path-boundary-safe prefix match at boot (the same
-- root_like_pattern the scanner sweep uses, so /media/movies does not claim
-- /media/movies-4k). NULL = not yet assigned / outside every configured
-- root.
CREATE TABLE IF NOT EXISTS libraries (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    root_path  TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,
    wire_id    TEXT NOT NULL,
    options    TEXT,
    created_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_libraries_wire_id ON libraries(wire_id);

ALTER TABLE media_items ADD COLUMN library_id INTEGER;

CREATE INDEX IF NOT EXISTS idx_media_items_library_id ON media_items(library_id);
