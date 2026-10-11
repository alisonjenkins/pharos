-- Only re-index a row in `media_fts` when a column the index reads changes.
--
-- 0029's `AFTER UPDATE ON media_items` fired on every update, so writing an
-- unrelated column (the `sort_name` backfill, a rescan touching `last_scanned`)
-- paid an fts5 'delete' + re-insert per row for no change to the index. The
-- index covers `title` and `overview` only, so `UPDATE OF title, overview`
-- is exactly the set of updates that can change it. The body is unchanged.
DROP TRIGGER IF EXISTS media_fts_au;

CREATE TRIGGER media_fts_au AFTER UPDATE OF title, overview ON media_items BEGIN
    INSERT INTO media_fts (media_fts, rowid, title, overview)
    VALUES ('delete', OLD.id, OLD.title, OLD.overview);
    INSERT INTO media_fts (rowid, title, overview)
    VALUES (NEW.id, NEW.title, NEW.overview);
END;
