-- LIB-B2: Unicode-case-folded title for SQL-side search + SortName.
--
-- The legacy in-memory `/Items` path matched + sorted titles via Rust's
-- `str::to_lowercase`, which is full-Unicode (É→é, Ü→ü). SQLite's built-in
-- `LOWER()` / `LIKE` only case-fold ASCII, so routing `/Items` through
-- `MediaStore::query` (LIB-B2) would silently drop accented matches
-- (regression caught by tests/unicode_search_audit.rs).
--
-- `title_fold` stores `to_lowercase(title)` computed in Rust at `put()`
-- time (the only Unicode-aware fold available without a Rust-in-SQLite
-- custom function). `query()` searches + sorts on
-- `COALESCE(title_fold, LOWER(title))`, so freshly-scanned rows fold the
-- full Unicode range and pre-0028 rows degrade to ASCII-fold until the next
-- rescan re-`put`s them (additive, no destructive backfill).
--
-- Indexed for the SortName ORDER BY + NameStartsWith prefix scans.
ALTER TABLE media_items ADD COLUMN title_fold TEXT;
CREATE INDEX IF NOT EXISTS idx_media_items_title_fold ON media_items(title_fold);
