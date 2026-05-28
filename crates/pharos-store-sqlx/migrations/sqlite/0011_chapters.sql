-- T54 phase 4 / T57 phase 2: persist the list of chapter markers
-- ffprobe `-show_chapters` discovered. JSON-blob shape, same pattern
-- as subtitle_tracks_json: empty arrays stored as NULL.
ALTER TABLE media_items ADD COLUMN chapters_json TEXT;
