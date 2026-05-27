-- T-fix-39: real DateCreated for SortBy=DateCreated/DateAdded.
-- Unix-seconds timestamp set on first insert; ON CONFLICT path
-- preserves the original (so re-running `pharos scan` doesn't reset
-- "added on" dates).
ALTER TABLE media_items ADD COLUMN created_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_media_items_created_at ON media_items(created_at);
