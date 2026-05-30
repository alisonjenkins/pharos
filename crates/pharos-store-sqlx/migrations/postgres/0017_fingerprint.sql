-- LIB-A6: content fingerprint (postgres mirror of the sqlite 0017
-- migration). BLOB -> BYTEA; raw 8-byte xxh3_64 digest. Additive +
-- nullable, rows predating this migration carry NULL until re-probe.
ALTER TABLE media_items ADD COLUMN fingerprint BYTEA;

CREATE INDEX IF NOT EXISTS idx_media_items_fingerprint ON media_items(fingerprint);
