-- What a metadata provider says about a person (biography, birth and death,
-- IMDb and Wikidata ids), kept so their page asks the provider at most once a
-- month. Keyed by the person's wire id, which outlives a person row rebuilt
-- by a rescan. A row with every field empty records that the provider had
-- nothing, so it is not asked again until it is stale.
CREATE TABLE IF NOT EXISTS person_details (
    wire_id     TEXT    PRIMARY KEY,
    biography   TEXT,
    birth_date  TEXT,
    death_date  TEXT,
    birth_place TEXT,
    imdb_id     TEXT,
    wikidata_id TEXT,
    fetched_at  BIGINT  NOT NULL
);
