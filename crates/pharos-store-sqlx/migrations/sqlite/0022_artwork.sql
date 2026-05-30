-- LIB-D4: sidecar artwork as real rows.
--
-- The Jellyfin image API previously served Primary/Backdrop/Thumb by
-- ffmpeg-extracting a frame from the source media on every cache miss, and
-- Logo/Banner/Disc/Art were upload-only (404 with no upload). We record
-- artwork discovered at scan time (sidecar poster.jpg / fanart.jpg /
-- logo.png / … beside the media, or under the series folder) so the
-- image-serving branch (D5) can serve a recorded local file directly
-- instead of re-globbing the filesystem or frame-extracting.
--
-- One row per (item_id, role): a given item carries at most one image per
-- role, and the highest-priority source wins on conflict (the resolver
-- feeds refs in provider-priority order; the scanner upserts, so a
-- later/lower-priority source does not clobber an already-recorded one
-- unless it is meant to). role = the ArtworkRole::as_str token
-- (Primary/Backdrop/Thumb/Logo/Banner/Disc/Art). source = 'local' | 'url'.
-- locator = absolute sidecar path (local) or remote URL (url).
--
-- No FK constraint on item_id (mirrors item_genres' additive-only shape);
-- a sweep that deletes the media_item leaves an orphan artwork row that the
-- read path simply never resolves (and a rescan replaces it). Indexed on
-- item_id so artwork_for(item) is a single indexed lookup.
CREATE TABLE IF NOT EXISTS artwork (
    item_id INTEGER NOT NULL,
    role    TEXT NOT NULL,
    source  TEXT NOT NULL,
    locator TEXT NOT NULL,
    PRIMARY KEY (item_id, role)
);

CREATE INDEX IF NOT EXISTS idx_artwork_item_id ON artwork(item_id);
