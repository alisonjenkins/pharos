-- P1: persist codec profile + level on media_items so HLS CODECS
-- attribute can be derived from the probe instead of the hardcoded
-- avc1.640028 string. Both columns nullable; older rows + non-AVC
-- codecs (vp9 / av1 / opus) report None.
ALTER TABLE media_items ADD COLUMN video_profile TEXT;
ALTER TABLE media_items ADD COLUMN video_level INTEGER;
