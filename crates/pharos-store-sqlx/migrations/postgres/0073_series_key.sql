-- Spec 033 (V893): an episode's series identity key. Until now it was
-- COALESCE(series_folder, series_name); a folder holding two same-named shows
-- is split by stated year into "{folder}\u{1f}{YYYY}" keys, which a folder
-- column cannot hold. Backfilled to today's key, so no wire id moves on deploy.
ALTER TABLE media_items ADD COLUMN series_key TEXT;
UPDATE media_items SET series_key = COALESCE(series_folder, series_name)
WHERE series_name IS NOT NULL;
-- NULL reads as the folder: rows inserted later carry a key only once split.
CREATE INDEX IF NOT EXISTS idx_media_items_series_key
    ON media_items(COALESCE(series_key, series_folder));

-- Spec 033 (V891): a show match whose runner-up was as good a reading is
-- flagged 'ambiguous' with the runner-up's id and score, for an admin to check.
ALTER TABLE series_metadata ADD COLUMN match_flag TEXT;
ALTER TABLE series_metadata ADD COLUMN match_runner_up_id TEXT;
ALTER TABLE series_metadata ADD COLUMN match_runner_up_score REAL;
