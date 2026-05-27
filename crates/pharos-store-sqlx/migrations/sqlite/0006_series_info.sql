-- T-fix-18: store-level Series / Season / Episode metadata on Episode
-- items. All nullable so Movies + Audio rows stay untouched.
ALTER TABLE media_items ADD COLUMN series_name TEXT;
ALTER TABLE media_items ADD COLUMN season_number INTEGER;
ALTER TABLE media_items ADD COLUMN episode_number INTEGER;
CREATE INDEX IF NOT EXISTS idx_media_items_series_name ON media_items(series_name);
