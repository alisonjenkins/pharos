-- LIB-A1: incremental-scan state + scan-run ledger.
-- Per-row fs-stat signature (mtime + size as seen on disk) drives the
-- A2 skip-unchanged path; `size_bytes` already on the row is the
-- ffprobe-reported FORMAT size, NOT the filesystem stat size, so a
-- dedicated `file_size_seen` column owns the stat value.
-- `last_seen_scan_id` ties a row to the most recent scan that observed
-- it; the A3 mark-and-sweep deletes rows whose id != the current run.
-- All additive (nullable ADD COLUMN / CREATE TABLE), matching 0010-0015.
ALTER TABLE media_items ADD COLUMN file_mtime INTEGER;
ALTER TABLE media_items ADD COLUMN file_size_seen INTEGER;
ALTER TABLE media_items ADD COLUMN last_scanned INTEGER;
ALTER TABLE media_items ADD COLUMN last_seen_scan_id INTEGER;

-- Append-only ledger: one row per scan run, for observability +
-- atomic mark-and-sweep tokens. `id` auto-assigns from rowid on NULL
-- insert.
CREATE TABLE IF NOT EXISTS scan_runs (
    id           INTEGER PRIMARY KEY,
    root         TEXT,
    started_at   INTEGER,
    finished_at  INTEGER,
    items_seen   INTEGER,
    items_swept  INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS idx_media_items_last_seen ON media_items(last_seen_scan_id);
