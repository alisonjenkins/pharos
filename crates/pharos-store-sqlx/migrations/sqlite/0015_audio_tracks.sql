-- P16: multi-audio track persistence. Stored as a JSON blob so each
-- track's full shape (codec, channels, language, title, default flag)
-- survives without schema churn when probe enrichment adds fields.
-- Nullable — older rows decode to an empty Vec.
ALTER TABLE media_items ADD COLUMN audio_tracks_json TEXT;
