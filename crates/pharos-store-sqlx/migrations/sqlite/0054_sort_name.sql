-- `SortBy=SortName` key: the title lowercased like `title_fold`, minus a
-- leading "the" / "a" / "an", with digit runs zero-padded, so "The Matrix"
-- files under M and "Ep 2" sorts before "Ep 10" (pharos_core::sort_name).
--
-- Additive and nullable. Rows that predate this migration stay NULL until
-- the post-migration backfill in `SqliteStore::connect` fills them in
-- batches; until then the query falls back to `title_fold`, so nothing
-- breaks mid-way and an older binary simply ignores the column.
ALTER TABLE media_items ADD COLUMN sort_name TEXT;
CREATE INDEX IF NOT EXISTS idx_media_items_sort_name ON media_items(sort_name);
