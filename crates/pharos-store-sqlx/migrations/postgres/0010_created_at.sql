-- T-fix-39: real DateCreated for SortBy=DateCreated/DateAdded.
ALTER TABLE media_items ADD COLUMN created_at BIGINT;
CREATE INDEX IF NOT EXISTS idx_media_items_created_at ON media_items(created_at);
