-- B341 — IMDb and Rotten Tomatoes scores from a ratings source (OMDb), for
-- films and shows. Written only by RatingsStore::set_ratings: a scan's put
-- and the series enricher's upsert never name these columns, so a rescan or
-- re-enrichment keeps them. ratings_fetched_at (unix seconds) is when the
-- last lookup ran, found or not; NULL = never looked up.
ALTER TABLE media_items ADD COLUMN imdb_rating REAL;
ALTER TABLE media_items ADD COLUMN rt_tomatometer BIGINT;
ALTER TABLE media_items ADD COLUMN ratings_fetched_at BIGINT;
ALTER TABLE series_metadata ADD COLUMN imdb_rating REAL;
ALTER TABLE series_metadata ADD COLUMN rt_tomatometer BIGINT;
ALTER TABLE series_metadata ADD COLUMN ratings_fetched_at BIGINT;
