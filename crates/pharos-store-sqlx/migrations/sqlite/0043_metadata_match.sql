-- 0043 — online-metadata match state (provider-agnostic). NULL match_source
-- = never matched = eligible for the background enricher. `manual`/`nfo_id`
-- are skipped by the enricher so a user override / local id is never clobbered.
ALTER TABLE media_items ADD COLUMN match_provider TEXT;
ALTER TABLE media_items ADD COLUMN match_external_id TEXT;
ALTER TABLE media_items ADD COLUMN match_source TEXT;
ALTER TABLE media_items ADD COLUMN match_confidence REAL;
ALTER TABLE media_items ADD COLUMN metadata_refreshed_at INTEGER;
