-- 004-books — admit 'book' to the `kind` CHECK constraint that migration 0001
-- created as `CHECK (kind IN ('movie', 'episode', 'audio'))`.
--
-- SQLite cannot ALTER a CHECK constraint. The officially supported route is the
-- 12-step table rebuild, which for `media_items` would mean transcribing 75
-- columns under STRICT plus 12 indexes and 3 FTS triggers, and rewriting every
-- row. One omitted column silently loses data.
--
-- Instead the schema TEXT is patched in place. This is `writable_schema`, which
-- SQLite documents as expert-only — the safety argument here is specific:
--
--   * `replace()` operates on the LIVE schema text, so the other 74 columns are
--     carried through by the database itself and cannot be lost in
--     transcription. Nothing is retyped.
--   * No row is read or written, so a large table costs the same as an empty
--     one and there is no partial-rewrite state to recover from.
--   * If the search string does not match, `replace()` is a no-op and the
--     schema is left byte-identical rather than corrupted. The migration then
--     fails loudly at the first attempt to store a book, not silently.
--   * `integrity_check` runs immediately after, inside the same migration.
--
-- `writable_schema = RESET` (SQLite 3.31+) reloads the schema cache, which plain
-- `OFF` does not — without it this connection would keep the pre-patch schema.
PRAGMA writable_schema = ON;

UPDATE sqlite_schema
   SET sql = replace(
         sql,
         'CHECK (kind IN (''movie'', ''episode'', ''audio''))',
         'CHECK (kind IN (''movie'', ''episode'', ''audio'', ''book''))'
       )
 WHERE type = 'table'
   AND name = 'media_items';

PRAGMA writable_schema = RESET;

-- Fails the migration if the patch left the database inconsistent.
PRAGMA integrity_check;
