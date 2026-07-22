-- 0043 — online-metadata match state (provider-agnostic). See sqlite/0043.
ALTER TABLE media_items ADD COLUMN match_provider TEXT;
ALTER TABLE media_items ADD COLUMN match_external_id TEXT;
ALTER TABLE media_items ADD COLUMN match_source TEXT;
ALTER TABLE media_items ADD COLUMN match_confidence REAL;
ALTER TABLE media_items ADD COLUMN metadata_refreshed_at BIGINT;
