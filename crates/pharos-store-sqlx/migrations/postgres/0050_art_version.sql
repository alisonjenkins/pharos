-- B124 — a cache-busting version for an item's artwork. `ImageTag` was derived
-- from (item id, role) alone, so replacing an item's art left every image URL
-- byte-identical while `Cache-Control: max-age=604800` kept clients on the old
-- picture for a week. Bumped whenever art BYTES are replaced (provider
-- download, manual upload, delete).
ALTER TABLE media_items ADD COLUMN IF NOT EXISTS art_version BIGINT NOT NULL DEFAULT 0;
