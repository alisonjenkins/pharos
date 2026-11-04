-- Spec 024: a song's tempo and energy, written by the background audio
-- analysis sweep, never by `put`. `audio_analysis_mtime`/`_size` are the file
-- signature the analysis read; a read uses the result only while it matches
-- the row's scan signature (`file_mtime`, `file_size_seen`), so a replaced
-- file reads as unanalysed with no extra write on the scan path (V385).
-- A row with a version and no values is an analysis that found nothing, kept
-- so the song is not decoded again every pass. BIGINT for postgres' strict INT8 decode.
ALTER TABLE media_items ADD COLUMN bpm_mille BIGINT;
ALTER TABLE media_items ADD COLUMN tempo_source TEXT;
ALTER TABLE media_items ADD COLUMN energy_mille BIGINT;
ALTER TABLE media_items ADD COLUMN audio_analysis_version BIGINT;
ALTER TABLE media_items ADD COLUMN audio_analysis_mtime BIGINT;
ALTER TABLE media_items ADD COLUMN audio_analysis_size BIGINT;
