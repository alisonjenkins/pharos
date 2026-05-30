-- LIB-B2: Unicode-case-folded title for SQL-side search + SortName.
-- Mirror of migrations/sqlite/0028_title_fold.sql.
--
-- Postgres `LOWER()` is already Unicode-aware, so the parity gap is
-- SQLite-only; the column is mirrored here so both backends share one
-- `query()` SQL shape (`COALESCE(title_fold, LOWER(title))`). `title_fold`
-- stores `to_lowercase(title)` computed in Rust at `put()` time.
ALTER TABLE media_items ADD COLUMN title_fold TEXT;
CREATE INDEX IF NOT EXISTS idx_media_items_title_fold ON media_items(title_fold);
