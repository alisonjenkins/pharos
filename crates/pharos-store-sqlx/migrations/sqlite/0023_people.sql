-- LIB-C2: people (cast & crew) as real entities.
--
-- /Persons previously returned an empty stub and BaseItemDto.People was
-- hardcoded `[]` — the MetadataResolver already parses NFO <actor> /
-- <director> / <credits> into PersonRef but the scanner logged-and-
-- dropped them for want of a table. We promote people to rows so they
-- carry a stable wire id (the 32-hex person_id_for(name) the Jellyfin
-- DTO emits as a Person's `Id`), so /Items?ParentId=<person id> is an
-- indexed join, and so an item's cast/crew round-trips onto its DTO.
--
-- people is name-keyed (one row per distinct person name); the entity
-- carries an optional sort_name (for name-ordered listing), the wire_id
-- (computed via pharos_core::person_wire_id at upsert), an optional
-- provider_ids blob (TMDB/IMDB person ids parsed from NFO, carried for a
-- later online-enrichment pass), and an optional thumb_url — the NFO
-- <actor><thumb> image URL, persisted per-person so the image API can
-- serve a cast headshot (the artwork table is keyed by media item id +
-- role, so a person headshot needs its own home).
--
-- item_people is the many-to-many join carrying the PER-LINK credit
-- detail a PersonRef holds: `role` (free-form NFO department string),
-- `character` (played character for cast), `person_kind` (the Jellyfin
-- PersonType token — Actor/Director/Writer/…), and `sort_order` (NFO
-- ordering). PK is (item_id, person_id, role) so the same person can
-- appear in two distinct credit roles on one item (e.g. director AND
-- writer) without collapsing.
--
-- No FK constraints (mirrors item_genres' additive-only shape); a swept
-- media_item leaves orphan join rows the read path never resolves (a
-- rescan replaces them). Indexed on person wire_id (ParentId pivot) +
-- both join columns.
CREATE TABLE IF NOT EXISTS people (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    sort_name    TEXT,
    wire_id      TEXT NOT NULL,
    provider_ids TEXT,
    thumb_url    TEXT
);

CREATE TABLE IF NOT EXISTS item_people (
    item_id     INTEGER NOT NULL,
    person_id   INTEGER NOT NULL,
    role        TEXT NOT NULL DEFAULT '',
    character   TEXT,
    person_kind TEXT NOT NULL DEFAULT 'Actor',
    sort_order  INTEGER,
    PRIMARY KEY (item_id, person_id, role)
);

CREATE INDEX IF NOT EXISTS idx_item_people_person_id ON item_people(person_id);
CREATE INDEX IF NOT EXISTS idx_item_people_item_id ON item_people(item_id);
CREATE INDEX IF NOT EXISTS idx_people_wire_id ON people(wire_id);
