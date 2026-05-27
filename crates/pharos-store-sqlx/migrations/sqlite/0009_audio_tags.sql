-- T-fix-32: audio file tags (artist / album / album_artist / genre)
-- from ffprobe format.tags. All nullable; non-audio items leave them
-- NULL.
ALTER TABLE media_items ADD COLUMN artist TEXT;
ALTER TABLE media_items ADD COLUMN album TEXT;
ALTER TABLE media_items ADD COLUMN album_artist TEXT;
ALTER TABLE media_items ADD COLUMN genre TEXT;
CREATE INDEX IF NOT EXISTS idx_media_items_artist ON media_items(artist);
CREATE INDEX IF NOT EXISTS idx_media_items_album_artist ON media_items(album_artist);
