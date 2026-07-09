-- Per-row probe schema version (pharos_core::PROBE_SCHEMA_VERSION at last
-- probe). The incremental scan re-probes a file whose (mtime,size) is unchanged
-- but whose stored version is older than current, so a probe-schema addition
-- backfills automatically on the next scan. Existing rows default to 0 (older
-- than any real version) → re-probed exactly once.
ALTER TABLE media_items ADD COLUMN probe_schema_version INTEGER NOT NULL DEFAULT 0;
