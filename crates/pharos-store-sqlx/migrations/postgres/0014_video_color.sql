-- P13: HDR + color pipeline metadata. All four columns nullable so
-- rows from older probe passes still load — HDR rendering defaults
-- to SDR when missing.
ALTER TABLE media_items ADD COLUMN pixel_format TEXT;
ALTER TABLE media_items ADD COLUMN color_primaries TEXT;
ALTER TABLE media_items ADD COLUMN color_transfer TEXT;
ALTER TABLE media_items ADD COLUMN color_space TEXT;
