-- Embedded attachment streams (fonts referenced by ASS/SSA subtitles).
-- Stored as a JSON blob, same pattern as subtitle_tracks_json /
-- audio_tracks_json. Nullable — older rows decode to an empty Vec.
-- jellyfin-web hands these to SubtitlesOctopus so styled subs render.
ALTER TABLE media_items ADD COLUMN attachments_json TEXT;
