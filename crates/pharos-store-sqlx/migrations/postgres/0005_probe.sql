-- T29-follow-up: persist real ffprobe metadata on media_items.
-- All columns nullable so rows imported pre-probe (or with probe failures)
-- still load — DTO layer omits None fields rather than fabricating values.
ALTER TABLE media_items ADD COLUMN size_bytes BIGINT;
ALTER TABLE media_items ADD COLUMN duration_ms BIGINT;
ALTER TABLE media_items ADD COLUMN container TEXT;
ALTER TABLE media_items ADD COLUMN bitrate_bps BIGINT;
ALTER TABLE media_items ADD COLUMN video_codec TEXT;
ALTER TABLE media_items ADD COLUMN audio_codec TEXT;
ALTER TABLE media_items ADD COLUMN width INTEGER;
ALTER TABLE media_items ADD COLUMN height INTEGER;
ALTER TABLE media_items ADD COLUMN frame_rate_mille INTEGER;
ALTER TABLE media_items ADD COLUMN audio_channels INTEGER;
ALTER TABLE media_items ADD COLUMN sample_rate INTEGER;
