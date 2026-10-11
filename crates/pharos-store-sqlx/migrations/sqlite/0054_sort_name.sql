-- `SortBy=SortName` key: the title lowercased like `title_fold`, minus a
-- leading "the" / "a" / "an", with digit runs zero-padded, so "The Matrix"
-- files under M and "Ep 2" sorts before "Ep 10" (pharos_core::sort_name).
--
-- Additive and nullable. Rows that predate this migration stay NULL until
-- they are backfilled; until then the query falls back to `title_fold`, so
-- nothing breaks mid-way and an older binary simply ignores the column.
-- Deliberately unindexed: the ORDER BY is a COALESCE over three columns,
-- which a plain index cannot serve.
ALTER TABLE media_items ADD COLUMN sort_name TEXT;
