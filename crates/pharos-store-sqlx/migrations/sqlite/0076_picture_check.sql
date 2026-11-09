-- The picture area inside baked-in black bars, written by the background
-- picture check sweep, never by `put`. `picture_check_mtime`/`_size` are the
-- file signature the check read; a read uses the result only while it matches
-- the row's scan signature (`file_mtime`, `file_size_seen`), as for audio
-- analysis (0064). A row with a version and no width is a check that found no
-- bars, kept so the file is not decoded again every pass. INTEGER: the table is STRICT.
ALTER TABLE media_items ADD COLUMN picture_check_version INTEGER;
ALTER TABLE media_items ADD COLUMN picture_x INTEGER;
ALTER TABLE media_items ADD COLUMN picture_y INTEGER;
ALTER TABLE media_items ADD COLUMN picture_w INTEGER;
ALTER TABLE media_items ADD COLUMN picture_h INTEGER;
ALTER TABLE media_items ADD COLUMN picture_check_mtime INTEGER;
ALTER TABLE media_items ADD COLUMN picture_check_size INTEGER;
