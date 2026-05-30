-- LIB-C11: folder-keyed series identity (postgres mirror of the sqlite
-- 0019 migration). TEXT -> TEXT, INTEGER -> INTEGER (series_year is a
-- small 4-digit year, mirroring production_year's column type). Additive
-- + nullable: rows predating this migration carry NULL and fall back to
-- the legacy name-keyed Series/Season identity.
ALTER TABLE media_items ADD COLUMN series_folder TEXT;
ALTER TABLE media_items ADD COLUMN series_year INTEGER;
