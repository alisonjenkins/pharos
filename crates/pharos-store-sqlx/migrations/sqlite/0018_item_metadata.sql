-- LIB-C7/C8/C9: item-level descriptive metadata (canonical superset).
-- These are NOT technical probe fields (MediaProbe stays codec/HDR-only,
-- core doc): they're descriptive metadata that EPIC D populates and the
-- Jellyfin BaseItemDto projects down (Overview, ratings, ProductionYear,
-- PremiereDate, Taglines, ProviderIds). Additive + nullable, matching
-- 0010-0017: rows predating this migration carry NULL.
--
-- premiere_date is unix-seconds (mirrors created_at); the DTO converts to
-- Jellyfin's ISO-8601 wire shape. provider_ids is a JSON object string
-- ({"tmdb":"…","imdb":"…"}) projected into BaseItemDto.ProviderIds.
ALTER TABLE media_items ADD COLUMN community_rating REAL;
ALTER TABLE media_items ADD COLUMN critic_rating REAL;
ALTER TABLE media_items ADD COLUMN official_rating TEXT;
ALTER TABLE media_items ADD COLUMN production_year INTEGER;
ALTER TABLE media_items ADD COLUMN premiere_date INTEGER;
ALTER TABLE media_items ADD COLUMN overview TEXT;
ALTER TABLE media_items ADD COLUMN tagline TEXT;
ALTER TABLE media_items ADD COLUMN provider_ids TEXT;
