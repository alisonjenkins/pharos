-- ADR-0062: a provider's title and description for a show or an episode, one
-- row per language. `owner_kind` is 'item' (an episode, `owner_key` = its
-- media id) or 'series' (`owner_key` = series_metadata.series_key). A row with
-- no title and no overview records a language the provider was asked for and
-- does not have, so it is not asked again until `fetched_at` ages out.
-- `is_original` marks the provider's base-record text (B564, V474).
-- `media_items.overview` / `series_metadata.overview` stay the default text
-- (English, else the original); these rows are what a user's description
-- language reads instead.
CREATE TABLE IF NOT EXISTS metadata_translations (
    owner_kind  TEXT NOT NULL,
    owner_key   TEXT NOT NULL,
    language    TEXT NOT NULL,
    title       TEXT,
    overview    TEXT,
    is_original BOOLEAN NOT NULL DEFAULT FALSE,
    fetched_at  BIGINT NOT NULL,
    PRIMARY KEY (owner_kind, owner_key, language)
);

CREATE INDEX IF NOT EXISTS idx_metadata_translations_language
    ON metadata_translations(language, owner_kind);
