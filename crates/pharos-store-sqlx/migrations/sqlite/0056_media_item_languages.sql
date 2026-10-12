-- Audio / subtitle languages as rows, so `AudioLanguages=` / `SubtitleLanguages=`
-- and their Filters2 facets read an index instead of scanning the track JSON
-- blobs with LIKE (pharos_core::language).
--
-- media_item_languages: one row per (kind, canonical ISO 639-2/B tag, item).
-- kind is 'audio' | 'subtitle'. The key leads with (kind, lang) so
-- `WHERE kind = ? AND lang IN (...)` and the facet's lang grouping are index
-- range scans; the item_id index serves the per-item delete. Rows are derived
-- from the blobs by the triggers below, so every writer keeps them right and
-- the application code never writes them; the cascade removes them with the
-- item.
--
-- language_codes: every spelling of a language (639-2/B, 639-2/T, 639-1) ->
-- its 639-2/B form, the table `pharos_core::language` holds in Rust (a test
-- asserts the two agree). A tag outside it is stored lower-cased as is.
CREATE TABLE language_codes (
    code  TEXT PRIMARY KEY,
    canon TEXT NOT NULL
) WITHOUT ROWID;

INSERT INTO language_codes (code, canon) VALUES
    ('alb', 'alb'),
    ('sqi', 'alb'),
    ('sq', 'alb'),
    ('arm', 'arm'),
    ('hye', 'arm'),
    ('hy', 'arm'),
    ('baq', 'baq'),
    ('eus', 'baq'),
    ('eu', 'baq'),
    ('bur', 'bur'),
    ('mya', 'bur'),
    ('my', 'bur'),
    ('chi', 'chi'),
    ('zho', 'chi'),
    ('zh', 'chi'),
    ('cze', 'cze'),
    ('ces', 'cze'),
    ('cs', 'cze'),
    ('dut', 'dut'),
    ('nld', 'dut'),
    ('nl', 'dut'),
    ('fre', 'fre'),
    ('fra', 'fre'),
    ('fr', 'fre'),
    ('geo', 'geo'),
    ('kat', 'geo'),
    ('ka', 'geo'),
    ('ger', 'ger'),
    ('deu', 'ger'),
    ('de', 'ger'),
    ('gre', 'gre'),
    ('ell', 'gre'),
    ('el', 'gre'),
    ('ice', 'ice'),
    ('isl', 'ice'),
    ('is', 'ice'),
    ('mac', 'mac'),
    ('mkd', 'mac'),
    ('mk', 'mac'),
    ('mao', 'mao'),
    ('mri', 'mao'),
    ('mi', 'mao'),
    ('may', 'may'),
    ('msa', 'may'),
    ('ms', 'may'),
    ('per', 'per'),
    ('fas', 'per'),
    ('fa', 'per'),
    ('rum', 'rum'),
    ('ron', 'rum'),
    ('ro', 'rum'),
    ('slo', 'slo'),
    ('slk', 'slo'),
    ('sk', 'slo'),
    ('tib', 'tib'),
    ('bod', 'tib'),
    ('bo', 'tib'),
    ('wel', 'wel'),
    ('cym', 'wel'),
    ('cy', 'wel'),
    ('ara', 'ara'),
    ('ar', 'ara'),
    ('cat', 'cat'),
    ('ca', 'cat'),
    ('dan', 'dan'),
    ('da', 'dan'),
    ('eng', 'eng'),
    ('en', 'eng'),
    ('fin', 'fin'),
    ('fi', 'fin'),
    ('heb', 'heb'),
    ('he', 'heb'),
    ('hin', 'hin'),
    ('hi', 'hin'),
    ('hun', 'hun'),
    ('hu', 'hun'),
    ('ind', 'ind'),
    ('id', 'ind'),
    ('ita', 'ita'),
    ('it', 'ita'),
    ('jpn', 'jpn'),
    ('ja', 'jpn'),
    ('kor', 'kor'),
    ('ko', 'kor'),
    ('nor', 'nor'),
    ('no', 'nor'),
    ('pol', 'pol'),
    ('pl', 'pol'),
    ('por', 'por'),
    ('pt', 'por'),
    ('rus', 'rus'),
    ('ru', 'rus'),
    ('spa', 'spa'),
    ('es', 'spa'),
    ('swe', 'swe'),
    ('sv', 'swe'),
    ('tha', 'tha'),
    ('th', 'tha'),
    ('tur', 'tur'),
    ('tr', 'tur'),
    ('ukr', 'ukr'),
    ('uk', 'ukr'),
    ('vie', 'vie'),
    ('vi', 'vie');

CREATE TABLE media_item_languages (
    kind    TEXT    NOT NULL,
    lang    TEXT    NOT NULL,
    item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    PRIMARY KEY (kind, lang, item_id)
) WITHOUT ROWID;
CREATE INDEX idx_media_item_languages_item ON media_item_languages(item_id);

-- Index the rows that already exist. One statement per kind; json_valid()
-- and json_type() keep a malformed or non-array blob (the Rust decoder reads
-- those as "no tracks") from aborting the migration or being iterated.
INSERT OR IGNORE INTO media_item_languages (kind, lang, item_id)
SELECT 'audio', COALESCE(c.canon, lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))), m.id
FROM media_items m,
     json_each(CASE WHEN json_valid(m.audio_tracks_json) THEN CASE WHEN json_type(m.audio_tracks_json) = 'array' THEN m.audio_tracks_json END END) j
LEFT JOIN language_codes c ON c.code = lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))
WHERE trim(coalesce((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END), '')) <> '';

INSERT OR IGNORE INTO media_item_languages (kind, lang, item_id)
SELECT 'subtitle', COALESCE(c.canon, lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))), m.id
FROM media_items m,
     json_each(CASE WHEN json_valid(m.subtitle_tracks_json) THEN CASE WHEN json_type(m.subtitle_tracks_json) = 'array' THEN m.subtitle_tracks_json END END) j
LEFT JOIN language_codes c ON c.code = lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))
WHERE trim(coalesce((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END), '')) <> '';

-- Keep them right for every writer, including a replica on an older binary:
-- a new item is indexed, a changed blob is re-indexed, an unchanged blob (a
-- rescan) costs nothing. Same statements as the backfill above.
CREATE TRIGGER media_items_languages_ai AFTER INSERT ON media_items
BEGIN
    INSERT OR IGNORE INTO media_item_languages (kind, lang, item_id)
    SELECT 'audio', COALESCE(c.canon, lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))), NEW.id
    FROM json_each(CASE WHEN json_valid(NEW.audio_tracks_json) THEN CASE WHEN json_type(NEW.audio_tracks_json) = 'array' THEN NEW.audio_tracks_json END END) j
    LEFT JOIN language_codes c ON c.code = lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))
    WHERE trim(coalesce((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END), '')) <> '';

    INSERT OR IGNORE INTO media_item_languages (kind, lang, item_id)
    SELECT 'subtitle', COALESCE(c.canon, lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))), NEW.id
    FROM json_each(CASE WHEN json_valid(NEW.subtitle_tracks_json) THEN CASE WHEN json_type(NEW.subtitle_tracks_json) = 'array' THEN NEW.subtitle_tracks_json END END) j
    LEFT JOIN language_codes c ON c.code = lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))
    WHERE trim(coalesce((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END), '')) <> '';
END;

CREATE TRIGGER media_items_languages_au
AFTER UPDATE OF audio_tracks_json, subtitle_tracks_json ON media_items
WHEN OLD.audio_tracks_json IS NOT NEW.audio_tracks_json
  OR OLD.subtitle_tracks_json IS NOT NEW.subtitle_tracks_json
BEGIN
    DELETE FROM media_item_languages WHERE item_id = NEW.id;

    INSERT OR IGNORE INTO media_item_languages (kind, lang, item_id)
    SELECT 'audio', COALESCE(c.canon, lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))), NEW.id
    FROM json_each(CASE WHEN json_valid(NEW.audio_tracks_json) THEN CASE WHEN json_type(NEW.audio_tracks_json) = 'array' THEN NEW.audio_tracks_json END END) j
    LEFT JOIN language_codes c ON c.code = lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))
    WHERE trim(coalesce((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END), '')) <> '';

    INSERT OR IGNORE INTO media_item_languages (kind, lang, item_id)
    SELECT 'subtitle', COALESCE(c.canon, lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))), NEW.id
    FROM json_each(CASE WHEN json_valid(NEW.subtitle_tracks_json) THEN CASE WHEN json_type(NEW.subtitle_tracks_json) = 'array' THEN NEW.subtitle_tracks_json END END) j
    LEFT JOIN language_codes c ON c.code = lower(trim((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END)))
    WHERE trim(coalesce((CASE WHEN j.type = 'object' THEN json_extract(j.value, '$.language') END), '')) <> '';
END;
