-- LIB-A6: content fingerprint for move/rename detection.
-- An 8-byte xxh3_64 digest over (file size, probed duration, head+tail
-- bytes) computed by the scanner (IO lives in pharos-scanner, never
-- core — V12). A renamed/moved file keeps its bytes, so its fingerprint
-- is stable even though its path-derived `stable_id` changes; the
-- scanner can therefore recognise a moved file as the same content
-- instead of import+sweep churn.
--
-- Stored as raw 8 bytes (BLOB). Additive + nullable, matching 0010-0016:
-- rows predating this migration simply carry NULL until next re-probe.
ALTER TABLE media_items ADD COLUMN fingerprint BLOB;

CREATE INDEX IF NOT EXISTS idx_media_items_fingerprint ON media_items(fingerprint);
