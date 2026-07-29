-- 004-books — book-specific metadata. Nullable with no backfill: every
-- existing row is a film, an episode or a track, and NULL book_format is
-- exactly "this item is not a book".
--
-- Deliberately NOT here: title, release date and description. Those are
-- ordinary item columns already, populated for a book by the same metadata
-- resolver that fills them for anything else (R6). Duplicating them per-kind
-- would give one field two authorities.
ALTER TABLE media_items ADD COLUMN book_format TEXT;
ALTER TABLE media_items ADD COLUMN book_page_count INTEGER;
ALTER TABLE media_items ADD COLUMN book_author TEXT;
ALTER TABLE media_items ADD COLUMN book_publisher TEXT;
ALTER TABLE media_items ADD COLUMN book_series TEXT;
-- NULL means "unnumbered volume", which sorts LAST within a series rather
-- than as 0 — unknown is not first.
ALTER TABLE media_items ADD COLUMN book_series_index INTEGER;
ALTER TABLE media_items ADD COLUMN book_isbn TEXT;
