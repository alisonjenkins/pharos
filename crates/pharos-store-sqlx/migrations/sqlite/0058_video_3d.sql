-- Stereoscopic 3D: packing of the primary file, plus a denormalised flag that
-- is true when the primary OR any grouped edition is 3D. The flag lets the
-- Is3D list filter stay a plain column predicate instead of scanning
-- alternate_sources_json. INTEGER (not BIGINT): the sqlite table is STRICT.
ALTER TABLE media_items ADD COLUMN video_3d_format TEXT;
ALTER TABLE media_items ADD COLUMN has_3d INTEGER NOT NULL DEFAULT 0;
