-- The language a SHOW was made in. TMDB/TVDB report it on the series record,
-- never on an episode — it is a property of the show — so an episode row's
-- own original_language is always NULL and the `OriginalLanguage` audio
-- preference could not work for TV at all. Episodes inherit this.
ALTER TABLE series_metadata ADD COLUMN IF NOT EXISTS original_language TEXT;
