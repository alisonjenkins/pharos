-- LIB-A1: incremental-scan state + scan-run ledger (postgres mirror of
-- the sqlite 0016 migration). INTEGER -> BIGINT, STRICT dropped,
-- BIGSERIAL for the auto-assigned scan_runs PK.
ALTER TABLE media_items ADD COLUMN file_mtime BIGINT;
ALTER TABLE media_items ADD COLUMN file_size_seen BIGINT;
ALTER TABLE media_items ADD COLUMN last_scanned BIGINT;
ALTER TABLE media_items ADD COLUMN last_seen_scan_id BIGINT;

CREATE TABLE IF NOT EXISTS scan_runs (
    id           BIGSERIAL PRIMARY KEY,
    root         TEXT,
    started_at   BIGINT,
    finished_at  BIGINT,
    items_seen   BIGINT,
    items_swept  BIGINT
);

CREATE INDEX IF NOT EXISTS idx_media_items_last_seen ON media_items(last_seen_scan_id);
