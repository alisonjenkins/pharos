-- LIB-C1: typed libraries as real entities (postgres mirror of the sqlite
-- 0021 migration). See the sqlite copy for the full rationale.
--
-- libraries.wire_id = the existing 32-hex library_id_for_root(root) the
-- views/virtual-folder DTOs emit as a library `Id`, stamped at reconcile
-- time so existing client URLs survive. media_items.library_id (nullable,
-- additive) is backfilled by a path-boundary-safe prefix match at boot.
--
-- Postgres `INTEGER PRIMARY KEY` does NOT autoincrement (unlike sqlite's
-- rowid alias), so libraries.id is a GENERATED IDENTITY column — the upsert
-- omits id and lets the backend assign it, then re-selects by the UNIQUE
-- root_path. media_items.library_id stays plain INTEGER (no FK constraint,
-- mirroring the sqlite shape + additive-only discipline).
CREATE TABLE IF NOT EXISTS libraries (
    id         INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT NOT NULL,
    root_path  TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,
    wire_id    TEXT NOT NULL,
    options    TEXT,
    created_at BIGINT
);

CREATE INDEX IF NOT EXISTS idx_libraries_wire_id ON libraries(wire_id);

ALTER TABLE media_items ADD COLUMN IF NOT EXISTS library_id INTEGER;

CREATE INDEX IF NOT EXISTS idx_media_items_library_id ON media_items(library_id);
