-- The language a title was made in, as the metadata provider states it
-- (TMDB gives ISO 639-1, e.g. `ja`). Drives the `OriginalLanguage` audio
-- preference: "play each title in its own language", the only way to want
-- Japanese for anime and English for everything else from one setting.
ALTER TABLE media_items ADD COLUMN IF NOT EXISTS original_language TEXT;
