-- LIB-C7/C8/C9: item-level descriptive metadata (postgres mirror of the
-- sqlite 0018 migration). REAL -> REAL, TEXT -> TEXT, INTEGER -> BIGINT
-- (premiere_date is unix-seconds, matching created_at's BIGINT). Additive
-- + nullable, rows predating this migration carry NULL until enrichment.
ALTER TABLE media_items ADD COLUMN community_rating REAL;
ALTER TABLE media_items ADD COLUMN critic_rating REAL;
ALTER TABLE media_items ADD COLUMN official_rating TEXT;
ALTER TABLE media_items ADD COLUMN production_year INTEGER;
ALTER TABLE media_items ADD COLUMN premiere_date BIGINT;
ALTER TABLE media_items ADD COLUMN overview TEXT;
ALTER TABLE media_items ADD COLUMN tagline TEXT;
ALTER TABLE media_items ADD COLUMN provider_ids TEXT;
