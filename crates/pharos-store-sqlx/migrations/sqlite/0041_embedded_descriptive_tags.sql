-- B90 embedded descriptive tags: the prober now reads a movie/episode's
-- embedded container tags — synopsis/description/comment (-> Overview),
-- content_rating/rating/mpaa (-> OfficialRating), network/publisher/studio
-- (-> Studios), and the full raw release_date (-> PremiereDate) — feeding the
-- `embedded` metadata provider for files without a sidecar NFO. Backfilled by
-- the incremental scan re-probe (PROBE_SCHEMA_VERSION 4).
ALTER TABLE media_items ADD COLUMN synopsis TEXT;
ALTER TABLE media_items ADD COLUMN content_rating TEXT;
ALTER TABLE media_items ADD COLUMN network TEXT;
ALTER TABLE media_items ADD COLUMN release_date TEXT;
