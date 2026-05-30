-- LIB-C2: people (cast & crew) as real entities (postgres mirror of the
-- sqlite 0023 migration). See the sqlite copy for the full rationale.
--
-- people.wire_id = the 32-hex person_id_for(name) the Jellyfin DTO emits,
-- computed at upsert time (pure hash in pharos-core, never IO). Indexed
-- so /Items?ParentId=<person id> resolves by an indexed join through
-- item_people rather than scanning every item's cast. item_people is the
-- many-to-many join carrying the per-link credit detail (role / character
-- / person_kind / sort_order); the probe carries no legacy people column,
-- so there is no backfill — the scanner populates the join from
-- MetadataResult on write.
--
-- Postgres `INTEGER PRIMARY KEY` does NOT autoincrement (unlike sqlite's
-- rowid alias), so people.id is a GENERATED IDENTITY column — the upsert
-- omits id and lets the backend assign it, then re-selects by the UNIQUE
-- name. item_people.person_id stays plain INTEGER (no FK constraint,
-- mirroring item_genres + additive-only discipline); item_id is BIGINT to
-- match media_items' u64 id range. PK (item_id, person_id, role) lets one
-- person hold two distinct credit roles on one item.
CREATE TABLE IF NOT EXISTS people (
    id           INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    sort_name    TEXT,
    wire_id      TEXT NOT NULL,
    provider_ids TEXT,
    thumb_url    TEXT
);

CREATE TABLE IF NOT EXISTS item_people (
    item_id     BIGINT NOT NULL,
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
