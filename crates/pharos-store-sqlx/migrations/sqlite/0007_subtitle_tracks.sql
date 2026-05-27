-- T-fix-19: persist the list of embedded subtitle tracks the prober
-- discovered. JSON-blob shape because the count + schema varies per
-- file and Jellyfin clients consume it as a freeform array anyway.
-- TEXT (not BLOB) so sqlite's CLI shows it readably; the contents are
-- always UTF-8 JSON.
ALTER TABLE media_items ADD COLUMN subtitle_tracks_json TEXT;
