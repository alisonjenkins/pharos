-- LIB-B3: indexes backing the SQL-side MediaStore::query() (LIB-B1).
--
-- query() resolves /Items entirely server-side: a dynamic WHERE (kind IN,
-- title substring/prefix, series_folder equality, EXISTS joins on the
-- item_<entity> tables by wire_id, a user_data join) + an allowlisted
-- ORDER BY + LIMIT/OFFSET + a COUNT(*) OVER () total. These indexes keep
-- the predicates and the common sort keys off a full table scan on a
-- 5–20k-row library.
--
-- Already present (no-op here, listed for the full picture):
--   idx_media_items_kind         (0001) — kind IN (…)
--   idx_media_items_created_at   (0010) — ORDER BY DateCreated
--   idx_media_items_series_name  (0006) — legacy series fallback
--   idx_media_items_artist /
--   idx_media_items_album_artist (0009) — Artist/Album pivots + sort
--   idx_media_items_library_id   (0021) — Library pivot
--   item_<entity>(<entity>_id) + (item_id) on every join table
--     (0020 genres, 0023 people, 0024 studios, 0025 collections, 0026 tags)
--
-- Added below:
--   title          — SortName (the default) ORDER BY + NameStartsWith /
--                    SearchTerm prefix scans.
--   series_folder  — the LIB-C11 folder-keyed Series / Season pivot
--                    (equality), distinct from the legacy series_name.
--   (kind, created_at) — the very common "movies/episodes newest-first"
--                    page: kind filter + DateCreated sort served by one
--                    composite index instead of an index-merge.
CREATE INDEX IF NOT EXISTS idx_media_items_title ON media_items(title);
CREATE INDEX IF NOT EXISTS idx_media_items_series_folder ON media_items(series_folder);
CREATE INDEX IF NOT EXISTS idx_media_items_kind_created_at ON media_items(kind, created_at);
