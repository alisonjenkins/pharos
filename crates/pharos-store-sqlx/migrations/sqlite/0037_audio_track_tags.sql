-- Audio tag enrichment (music browse): track/disc numbers drive album
-- track ordering (wire IndexNumber/ParentIndexNumber); release_year
-- drives the PremiereDate/ProductionYear album sort. Backfilled by the
-- incremental scan re-probe (PROBE_SCHEMA_VERSION 2).
ALTER TABLE media_items ADD COLUMN track_number INTEGER;
ALTER TABLE media_items ADD COLUMN disc_number INTEGER;
ALTER TABLE media_items ADD COLUMN release_year INTEGER;
