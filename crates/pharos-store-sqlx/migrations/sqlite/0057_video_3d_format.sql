-- 009-3d-media — stereoscopic 3D layout metadata (HalfSideBySide,
-- HalfTopAndBottom, etc). Nullable with no backfill: NULL indicates standard
-- 2D media or unprobed/unspecified format.
--
-- Partial index accelerates Is3D filter queries while imposing zero index cost
-- on 2D library items.
ALTER TABLE media_items ADD COLUMN video_3d_format TEXT;

CREATE INDEX IF NOT EXISTS idx_media_items_video_3d_format
    ON media_items (video_3d_format)
    WHERE video_3d_format IS NOT NULL;
