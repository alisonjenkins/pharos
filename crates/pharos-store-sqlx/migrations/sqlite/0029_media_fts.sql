-- LIB-B4: full-text search over title + overview (SQLite fts5).
--
-- /Search/Hints previously scanned the whole library in memory and kept
-- only rows whose `title` contained the (case-folded) needle — O(n) per
-- keystroke, title-only, and substring-only (no prefix ranking). We back
-- it with an fts5 external-content virtual table so the match is an
-- indexed, ranked (bm25), prefix-friendly scan over title AND overview.
--
-- `content='media_items', content_rowid='id'` makes media_fts an
-- EXTERNAL-CONTENT index: fts5 stores only the inverted index, not a copy
-- of the text — it reads the columns back out of media_items by rowid. The
-- three triggers below keep the index in lock-step with media_items on
-- INSERT / UPDATE / DELETE. For external-content tables the UPDATE/DELETE
-- triggers must issue the special 'delete' command (writing the OLD column
-- values) BEFORE re-inserting, or the index keeps stale postings — this is
-- the canonical fts5 external-content sync pattern.
--
-- The matcher is unicode61 (the fts5 default): tokens fold Unicode case +
-- diacritics and match as whole tokens; the search code appends a `*`
-- prefix marker per token so `pok*` finds "Pokemon". Pure mid-word
-- substrings the tokenizer can't reach (e.g. "kemon" inside "Pokemon") are
-- covered by the substring arm the search() code UNIONs in, so the FTS
-- result is always a SUPERSET of the legacy substring match.
CREATE VIRTUAL TABLE IF NOT EXISTS media_fts USING fts5(
    title,
    overview,
    content='media_items',
    content_rowid='id'
);

-- Keep the fts index synced with media_items. NEW.* feeds the insert;
-- OLD.* feeds the special 'delete' command (external-content tables can't
-- read the pre-image themselves).
CREATE TRIGGER IF NOT EXISTS media_fts_ai AFTER INSERT ON media_items BEGIN
    INSERT INTO media_fts (rowid, title, overview)
    VALUES (NEW.id, NEW.title, NEW.overview);
END;

CREATE TRIGGER IF NOT EXISTS media_fts_ad AFTER DELETE ON media_items BEGIN
    INSERT INTO media_fts (media_fts, rowid, title, overview)
    VALUES ('delete', OLD.id, OLD.title, OLD.overview);
END;

CREATE TRIGGER IF NOT EXISTS media_fts_au AFTER UPDATE ON media_items BEGIN
    INSERT INTO media_fts (media_fts, rowid, title, overview)
    VALUES ('delete', OLD.id, OLD.title, OLD.overview);
    INSERT INTO media_fts (rowid, title, overview)
    VALUES (NEW.id, NEW.title, NEW.overview);
END;

-- One-time backfill: rebuild the index from the existing media_items rows
-- (the triggers only fire on future writes). 'rebuild' is the fts5
-- external-content command that re-derives every posting from the content
-- table; safe + idempotent.
INSERT INTO media_fts (media_fts) VALUES ('rebuild');
