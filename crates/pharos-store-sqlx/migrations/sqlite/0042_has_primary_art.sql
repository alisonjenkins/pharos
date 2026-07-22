-- Denormalized "this item has a servable local Primary image" flag. The
-- Jellyfin DTO advertises an `ImageTags.Primary` for every item; for video a
-- frame-extract always satisfies it, but an Audio track's only Primary source
-- is a local sidecar (`poster`/`folder`/`cover.jpg`) recorded in the `artwork`
-- table. A coverless track therefore promised a poster the image route could
-- never serve → a guaranteed 404 on every grid render. This flag lets
-- `image_tags_for` advertise the audio Primary tag only when one truly exists,
-- so the invalid "advertised but unservable" state can't be represented.
--
-- Maintained by `set_artwork` (the only writer of the `artwork` table): a
-- `role='Primary'` write sets the flag to whether its source is local. Backfill
-- existing rows from the current artwork table so no rescan is needed.
ALTER TABLE media_items ADD COLUMN has_primary_art INTEGER NOT NULL DEFAULT 0;

UPDATE media_items SET has_primary_art = 1
 WHERE id IN (SELECT item_id FROM artwork WHERE role = 'Primary' AND source = 'local');
