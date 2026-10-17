-- Persist MediaProbe::alternate_sources (edition files grouped under one item).
-- Before this the scanner built them in memory and every reload dropped them.
-- JSON blob, empty list stored as NULL, same pattern as chapters_json.
ALTER TABLE media_items ADD COLUMN alternate_sources_json TEXT;
